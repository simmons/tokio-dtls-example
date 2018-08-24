//! This module provides practical tools for using UDP in Tokio.  The `UdpServer` struct actually
//! owns a UDP server socket, and dispatches to child `UdpStream` pseudo-sockets as needed
//! according to the peer address and port number.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::Bytes;
use futures::sync::mpsc;
use futures::{Async, AsyncSink, Poll, Sink, StartSend, Stream};
use tokio;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UdpSocket;

type Datagram = (SocketAddr, Bytes);

// A typical MTU (including IP and UDP headers) is 1500, but we don't actually address MTU concerns
// in this code.
const MAX_DATAGRAM: usize = 2048;

// We can buffer this many datagrams in the server for each child.
const UDP_STREAM_BUFFER_DATAGRAMS: usize = 8;

/// A `UdpServer` (de-)multiplexes a single UDP socket to allow multiple UDP data flows to distinct
/// clients, as determined by their address and port numbers.  As a `Stream`, `UdpServer` yields
/// new `UdpStream` objects which represent communication with a single peer.
pub struct UdpServer {
    socket: UdpSocket,
    streams: HashMap<SocketAddr, UdpChild>,
    incoming_datagram: Option<Datagram>,
}

impl UdpServer {
    /// Create a new `UdpServer` bound to the provided local address.
    pub fn bind(addr: &SocketAddr) -> Result<UdpServer, tokio::io::Error> {
        info!("Binding UDP socket: {:?}", addr);
        Ok(UdpServer {
            socket: UdpSocket::bind(addr)?,
            streams: HashMap::new(),
            incoming_datagram: None,
        })
    }

    /// Fetch the existing UdpChild associated with the provided address and port number, or create
    /// a new one.  Return the child and optionally the `UdpStream`, if one was created.
    ///
    /// It's a shame we can't use `&mut self` here -- the caller would have to borrow the entire
    /// `UdpServer` state, and be unable to use other fields until the returned reference went out
    /// of scope.
    fn get_or_create_child(
        streams: &mut HashMap<SocketAddr, UdpChild>,
        address: SocketAddr,
    ) -> (&mut UdpChild, Option<UdpStream>) {
        match streams.entry(address) {
            Entry::Occupied(o) => (o.into_mut(), None),
            Entry::Vacant(v) => {
                let (mut incoming_tx, incoming_rx) = mpsc::channel(UDP_STREAM_BUFFER_DATAGRAMS);
                let (outgoing_tx, outgoing_rx) = mpsc::channel(UDP_STREAM_BUFFER_DATAGRAMS);

                let stream = UdpStream {
                    peer: address,
                    outgoing_tx,
                    incoming_rx,
                };

                let child = v.insert(UdpChild {
                    incoming_tx,
                    outgoing_rx,
                    outgoing_datagram: None,
                });
                (child, Some(stream))
            }
        }
    }
}

impl Stream for UdpServer {
    type Item = UdpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<UdpStream>, io::Error> {
        // Process outgoing datagrams for each child
        let mut remove_peers: Vec<SocketAddr> = vec![];
        'outer: for (peer, child) in self.streams.iter_mut() {
            // Process any pending datagrams first.
            if let Some(datagram) = child.outgoing_datagram.take() {
                match self.socket.poll_send_to(&datagram.1, &datagram.0) {
                    Ok(Async::Ready(_)) => {}
                    Ok(Async::NotReady) => {
                        // Try again later.
                        child.outgoing_datagram = Some(datagram);
                        break;
                    }
                    Err(e) => {
                        error!("Error sending to socket: {:?}", e);
                    }
                }
            }

            // Pull datagrams from the outgoing queue to send.
            loop {
                match child.outgoing_rx.poll() {
                    Ok(Async::Ready(Some(datagram))) => {
                        match self.socket.poll_send_to(&datagram.1, &datagram.0) {
                            Ok(Async::Ready(_)) => {}
                            Ok(Async::NotReady) => {
                                // Try again later.
                                child.outgoing_datagram = Some(datagram);
                                break 'outer;
                            }
                            Err(e) => {
                                error!("Error sending to socket: {:?}", e);
                            }
                        }
                    }
                    Ok(Async::Ready(None)) => {
                        remove_peers.push(*peer);
                        continue 'outer;
                    }
                    Ok(Async::NotReady) => break,
                    Err(e) => {
                        error!("Error dequeueing outgoing datagram: {:?}", e);
                        break 'outer;
                    }
                };
            }
        }
        for peer in remove_peers {
            info!("UdpServer: Removing peer: {:?}", peer);
            self.streams.remove(&peer);
        }

        // Process any incoming datagrams and dispatch to child streams according to their address
        // and port numbers.
        let mut buffer: [u8; MAX_DATAGRAM] = [0; MAX_DATAGRAM];
        loop {
            // Fetch the next datagram to process -- either a previously parked incoming datagram,
            // or the next polled datagram from the stream (if available).
            let (address, payload) = match self.incoming_datagram.take() {
                Some(datagram) => datagram,
                None => match self.socket.poll_recv_from(&mut buffer) {
                    Ok(Async::Ready((nbytes, address))) => {
                        let payload: Bytes = buffer[0..nbytes].into();
                        (address, payload)
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(e) => {
                        error!("UdpServer: error: {:?}", e);
                        return Err(e);
                    }
                },
            };

            let mut remove = false;
            let new_stream;
            {
                // Find the child stream for this peer address/port, or create a new one.
                let (child, stream) = Self::get_or_create_child(&mut self.streams, address);
                new_stream = stream;

                // Dispatch the incoming datagram to the stream.
                debug!(
                    "dispatching {} bytes of payload from {}.",
                    payload.len(),
                    address
                );
                trace!("Payload:\n{}", ::util::hex(&payload));
                match child.incoming_tx.try_send(payload) {
                    Ok(()) => {}
                    Err(e) => {
                        if e.is_full() {
                            // Park the incoming datagram until we can send it.
                            self.incoming_datagram = Some((address, e.into_inner()));
                            // Don't process any more incoming datagrams until the parked datagram
                            // is processed.  (Otherwise we might drop our parked datagram if
                            // another incoming datagram needs parking.)  This does mean that one
                            // stream that is slow to process can block all the others.
                            return Ok(Async::NotReady);
                        } else {
                            error!("Error: Can't send to child stream: {:?} (closing)", e);
                            remove = true;
                        }
                    }
                };
            }
            if remove {
                self.streams.remove(&address);
            } else if let Some(new_stream) = new_stream {
                // Yield the new child stream, if one was created above.
                return Ok(Async::Ready(Some(new_stream)));
            }
        }
    }
}

/// This is the private child, owned by the `UdpServer`, used for communicating with the
/// `UdpStream` supplied to the caller.
struct UdpChild {
    incoming_tx: mpsc::Sender<Bytes>,
    outgoing_rx: mpsc::Receiver<Datagram>,
    outgoing_datagram: Option<Datagram>,
}

/// Provide a virtual UDP socket which communicates with a single peer via the UdpServer.
pub struct UdpStream {
    peer: SocketAddr,
    outgoing_tx: mpsc::Sender<(SocketAddr, Bytes)>,
    incoming_rx: mpsc::Receiver<Bytes>,
}

impl Stream for UdpStream {
    type Item = Bytes;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Bytes>, io::Error> {
        let payload = match self.incoming_rx.poll() {
            Ok(Async::Ready(Some(t))) => t,
            Ok(Async::Ready(None)) => {
                info!("Socket closing.");
                return Ok(Async::Ready(None));
            }
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Err(e) => {
                error!("Error receiving incoming datagram: {:?}", e);
                return Ok(Async::NotReady);
            }
        };

        debug!("received payload: {} bytes", payload.len());

        Ok(Async::Ready(Some(payload)))
    }
}

impl Sink for UdpStream {
    type SinkItem = Bytes;
    type SinkError = io::Error;

    fn start_send(&mut self, payload: Bytes) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.outgoing_tx.start_send((self.peer, payload))
            .map(|async_sink| async_sink.map(|(_peer,payload)| payload))
            // An error usually means the receiver is dropped; i.e. socket is closed.
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.outgoing_tx
            .poll_complete()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

impl Read for UdpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.poll() {
            Ok(Async::Ready(Some(msg))) => {
                let nbytes = msg.len();
                if nbytes > buf.len() {
                    return Err(io::Error::new(io::ErrorKind::Other, "buffer overrun"));
                }
                buf[..nbytes].copy_from_slice(&msg[..]);
                Ok(nbytes)
            }
            Ok(Async::Ready(None)) => Ok(0),
            Ok(Async::NotReady) => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
            Err(e) => Err(e),
        }
    }
}

impl AsyncRead for UdpStream {}

impl Write for UdpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.start_send(buf.into()) {
            Ok(AsyncSink::Ready) => Ok(buf.len()),
            Ok(AsyncSink::NotReady(_)) => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
            Err(e) => Err(e),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncWrite for UdpStream {
    fn shutdown(&mut self) -> Result<Async<()>, tokio::io::Error> {
        Ok(Async::Ready(()))
    }
}

/// A newtype which wraps `UdpSocket` to provide `AsyncRead` and `AsyncWrite` traits.
pub struct UdpAsyncSocket(UdpSocket);

impl UdpAsyncSocket {
    pub fn connect(address: &SocketAddr) -> Result<UdpAsyncSocket, io::Error> {
        #[inline]
        fn inaddr_any() -> IpAddr {
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
        }

        #[inline]
        fn in6addr_any() -> IpAddr {
            IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0))
        }

        let bind_address = match address {
            SocketAddr::V4(_) => SocketAddr::new(inaddr_any(), 0),
            SocketAddr::V6(_) => SocketAddr::new(in6addr_any(), 0),
        };
        let socket = UdpSocket::bind(&bind_address)?;
        socket.connect(address)?;
        Ok(UdpAsyncSocket(socket))
    }
}

impl Read for UdpAsyncSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.poll_recv(buf) {
            Ok(Async::Ready(nbytes)) => Ok(nbytes),
            Ok(Async::NotReady) => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
            Err(e) => Err(e),
        }
    }
}

impl AsyncRead for UdpAsyncSocket {}

impl Write for UdpAsyncSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.poll_send(buf) {
            Ok(Async::Ready(nbytes)) => Ok(nbytes),
            Ok(Async::NotReady) => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
            Err(e) => Err(e),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncWrite for UdpAsyncSocket {
    fn shutdown(&mut self) -> Result<Async<()>, tokio::io::Error> {
        Ok(Async::Ready(()))
    }
}
