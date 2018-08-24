//! Various utilities.

use std::fmt;
use std::io;

use bytes::Bytes;
use futures::{Async, AsyncSink, Poll, Sink, StartSend};
use tokio::io::AsyncWrite;

/// Wrap an `AsyncWrite` in a `Sink`, so it can be used as the target of a `Stream.forward()`.
pub struct AsyncWriteSink<T> {
    writer: T,
}

impl<T> AsyncWriteSink<T>
where
    T: AsyncWrite,
{
    pub fn new(writer: T) -> AsyncWriteSink<T> {
        AsyncWriteSink { writer }
    }
}

impl<T> Sink for AsyncWriteSink<T>
where
    T: AsyncWrite,
{
    type SinkItem = Bytes;
    type SinkError = io::Error;

    fn start_send(&mut self, payload: Bytes) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.writer.poll_write(&payload) {
            Ok(Async::Ready(nbytes)) => {
                if nbytes != payload.len() {
                    // With datagrams, it's an error if we can't write the entire buffer.
                    Err(io::Error::new(io::ErrorKind::Other, "buffer overrun"))
                } else {
                    Ok(AsyncSink::Ready)
                }
            }
            Ok(Async::NotReady) => Ok(AsyncSink::NotReady(payload)),
            Err(e) => Err(e),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.writer.poll_flush()
    }
}

/// Wrap a `Sink` and arrange for it to be flushed after every write.  Because we are dealing with
/// datagrams, it's important we don't try to buffer bytes as if they were part of a continuous
/// stream.
pub struct FlushingSink<S> {
    sink: S,
    needs_flush: bool,
}
impl<S> FlushingSink<S>
where
    S: Sink,
{
    pub fn new(sink: S) -> FlushingSink<S> {
        FlushingSink {
            sink,
            needs_flush: false,
        }
    }
}

impl<S> Sink for FlushingSink<S>
where
    S: Sink,
{
    type SinkItem = S::SinkItem;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        if self.needs_flush {
            match self.sink.poll_complete() {
                Ok(Async::Ready(_)) => self.needs_flush = false,
                Ok(Async::NotReady) => return Ok(AsyncSink::NotReady(item)),
                Err(e) => return Err(e),
            }
        }

        match self.sink.start_send(item) {
            Ok(AsyncSink::Ready) => self.needs_flush = true,
            Ok(AsyncSink::NotReady(t)) => return Ok(AsyncSink::NotReady(t)),
            Err(e) => return Err(e),
        };

        match self.sink.poll_complete() {
            Ok(Async::Ready(_)) => self.needs_flush = false,
            Ok(Async::NotReady) => {} // Can't flush now; try again next time
            Err(e) => return Err(e),
        };
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sink.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.sink.close()
    }
}

/// Generate a hexdump of the provided byte slice.
pub fn hexdump(f: &mut fmt::Formatter, prefix: &str, buffer: &[u8]) -> Result<(), fmt::Error> {
    const COLUMNS: usize = 16;
    let mut offset: usize = 0;
    if buffer.len() == 0 {
        // For a zero-length buffer, at least print an offset instead of
        // nothing.
        try!(write!(f, "{}{:04x}: ", prefix, 0));
    }
    while offset < buffer.len() {
        try!(write!(f, "{}{:04x}: ", prefix, offset));

        // Determine row byte range
        let next_offset = offset + COLUMNS;
        let (row_size, padding) = if next_offset <= buffer.len() {
            (COLUMNS, 0)
        } else {
            (buffer.len() - offset, next_offset - buffer.len())
        };
        let row = &buffer[offset..offset + row_size];

        // Print hex representation
        for b in row {
            try!(write!(f, "{:02x} ", b));
        }
        for _ in 0..padding {
            try!(write!(f, "   "));
        }

        // Print ASCII representation
        for b in row {
            try!(write!(
                f,
                "{}",
                match *b {
                    c @ 0x20...0x7E => c as char,
                    _ => '.',
                }
            ));
        }

        offset += COLUMNS;
        if offset < buffer.len() {
            try!(writeln!(f, ""));
        }
    }
    Ok(())
}

/// A byte slice wrapped in Hex is printable as a hex dump.
#[allow(dead_code)]
pub struct Hex<'a>(pub &'a [u8]);
impl<'a> fmt::Display for Hex<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        hexdump(f, "", self.0)
    }
}

/// Wrap the provided byte slice in a Hex to allow it to be printable as a hex dump.
pub fn hex(bytes: &[u8]) -> Hex {
    Hex(bytes)
}
