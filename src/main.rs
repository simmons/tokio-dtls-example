//! This program is an example showing how DTLS can be used in Tokio.

extern crate bytes;
extern crate env_logger;
extern crate futures;
#[macro_use]
extern crate log;
extern crate openssl;
extern crate tokio;
extern crate tokio_codec;
extern crate tokio_openssl;

mod udp;
mod util;

use std::io;
use std::net::SocketAddr;

use bytes::BytesMut;
use futures::Future;
use futures::Stream;
use openssl::pkey::PKey;
use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;
use tokio_codec::Decoder;
use tokio_openssl::{SslAcceptorExt, SslConnectorExt};

use udp::{UdpAsyncSocket, UdpServer};
use util::{AsyncWriteSink, FlushingSink};

/// The certificate which the server will provide.
static SERVER_CERT: &'static [u8] = include_bytes!("server-cert.pem");
/// The private key used by the server.
static SERVER_KEY: &'static [u8] = include_bytes!("server-key.pem");
/// When in client mode, we provide this domain name to OpenSSL for the purposes of verifying the
/// domain name embedded in the server certificate.  (Since we don't actually verify the
/// certificate, this isn't actually used for anything.)
static SERVER_DOMAIN: &'static str = "server";

/// Provision an `SslConnector` for clients.
fn ssl_connector() -> Result<SslConnector, io::Error> {
    let mut connector_builder = SslConnector::builder(SslMethod::dtls())?;
    // Disable certificate checking, since it's not practical to make users deal with public key
    // infrastructure just to see this example code work.
    // OMG DON'T EVER DO THIS IN PRODUCTION CODE!  CUT-AND-PASTE ENTHUSIASTS BE WARNED!
    // Certificate verification is critical to avoid man-in-the-middle attacks!
    connector_builder.set_verify(SslVerifyMode::NONE);
    let connector = connector_builder.build();
    Ok(connector)
}

/// Provision an `SslAcceptor` for servers.
fn ssl_acceptor(certificate: &[u8], private_key: &[u8]) -> Result<SslAcceptor, io::Error> {
    let mut acceptor_builder = SslAcceptor::mozilla_intermediate(SslMethod::dtls())?;
    acceptor_builder.set_certificate(&&X509::from_pem(certificate)?)?;
    acceptor_builder.set_private_key(&&PKey::private_key_from_pem(private_key)?)?;
    acceptor_builder.check_private_key()?;
    let acceptor = acceptor_builder.build();
    Ok(acceptor)
}

/// Run as a client by connecting to the peer, performing the DTLS handshake, and forwarding
/// to/from standard output/input.
fn client<S: Into<String>>(address: &SocketAddr, domain: S) -> Result<(), io::Error> {
    let connector = ssl_connector()?;
    let domain = domain.into();

    let socket = UdpAsyncSocket::connect(address)?;
    let client = futures::future::ok(()).and_then(move |_| {
        connector
            .connect_async(&domain, socket)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
            .and_then(|dtls_stream| {
                // Perform line-buffered copy from stdin to the DTLS stream, and datagram-buffered
                // copy from the DTLS stream to stdout.
                let (dtls_sink, dtls_stream) =
                    tokio_codec::BytesCodec::new().framed(dtls_stream).split();
                let future_out = tokio::io::lines(io::BufReader::new(tokio::io::stdin()))
                    .map(|mut l| {
                        l.push('\n');
                        l.into()
                    }).forward(FlushingSink::new(dtls_sink));
                let future_in = dtls_stream
                    .map(|b| b.freeze())
                    .forward(AsyncWriteSink::new(tokio::io::stdout()));

                // Spawn the inbound flow as a separate task, and use the outbound future in this
                // task.
                tokio::spawn(future_in.map(|_| ()).map_err(|_| ()));
                future_out.map(|_| ())
            }).map_err(|error| {
                println!("client error: {:?}", error);
            })
    });

    tokio::run(client);

    Ok(())
}

/// Run a simple DTLS echo server.
fn server(bind_address: &SocketAddr) -> Result<(), io::Error> {
    let acceptor = ssl_acceptor(SERVER_CERT, SERVER_KEY)?;

    // Bind the server's socket
    let udp = UdpServer::bind(bind_address)?;
    let server = udp
        .for_each(move |stream| {
            tokio::spawn(
                acceptor
                    .accept_async(stream)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                    .and_then(|dtls_stream| {
                        let codec = tokio_codec::BytesCodec::new();
                        let (sink, stream) = codec.framed(dtls_stream).split();
                        stream
                            .map(|b| {
                                const PREFIX: &[u8] = b"Echo: ";
                                let mut echo = BytesMut::with_capacity(PREFIX.len() + b.len());
                                echo.extend_from_slice(PREFIX);
                                echo.extend_from_slice(&b);
                                echo.into()
                            }).forward(sink)
                    }).map(|_| ())
                    .map_err(|_| ()),
            );

            Ok(())
        }).map_err(|error| {
            println!("server error: {:?}", error);
        });

    tokio::run(server);

    Ok(())
}

/// Parse the command-line arguments.
fn parse_arguments() -> Result<(bool, SocketAddr), &'static str> {
    let mut args = std::env::args();
    args.next().ok_or("Bad argument list")?;
    let is_server = match args
        .next()
        .ok_or("Missing server/client argument")?
        .as_ref()
    {
        "server" => true,
        "client" => false,
        _ => return Err("Bad server/client argument"),
    };
    let address = args
        .next()
        .ok_or("Missing address:port argument")?
        .parse()
        .map_err(|_| "Bad address:port argument")?;
    Ok((is_server, address))
}

fn main() -> Result<(), String> {
    env_logger::init();

    let (is_server, address) = parse_arguments().or_else(|e| {
        println!("Usage:");
        println!("\ttokio-dtls-example server 127.0.0.1:9000");
        println!("\ttokio-dtls-example client 127.0.0.1:9000");
        Err(e)
    })?;

    if is_server {
        server(&address).map_err(|e| format!("I/O error: {}", e))?;
    } else {
        client(&address, SERVER_DOMAIN).map_err(|e| format!("I/O error: {}", e))?;
    }
    Ok(())
}
