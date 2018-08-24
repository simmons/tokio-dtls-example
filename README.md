tokio-dtls-example: Using Datagram TLS in Tokio
======================================================================

The problem
----------------------------------------

While recently trying to implement a DTLS protocol in Tokio, I ran into
some friction:

1. Using DTLS (Datagram TLS, i.e. TLS over UDP) in Tokio is not well
   documented, and I couldn't find a clear example.
2. The [`native-tls`](https://crates.io/crates/native-tls) crate, which
   provides a platform-independent API to your system's local TLS
   library (e.g. OpenSSL, Mac OS security-framework, or Windows
   SChannel), does not currently support DTLS.
3. By extension, the [`tokio-tls`](https://crates.io/crates/tokio-tls)
   crate, which relies on `native-tls`, also does not support DTLS.
   (See [this old
   issue](https://github.com/tokio-rs/tokio-tls/issues/13).)

The solution
----------------------------------------

OpenSSL itself supports DTLS, of course.  If you are willing to have a
dependency on OpenSSL, this is always an option.  The Rust
[`openssl`](https://crates.io/crates/openssl) crate supports all of the
OpenSSL API required for DTLS.
The final piece of the puzzle is Alex Crichton's
[`tokio-openssl`](https://crates.io/crates/tokio-openssl) crate, which
is small in size but big on the magic needed to use OpenSSL from Tokio.

The example
----------------------------------------

This repository provides a simple program implementing a DTLS client and
server.  The client forwards lines from standard input to the peer, and
prints data from the peer to standard output.  The server echos each
line back to the client with "Echo: " prepended.

While UDP itself is connectionless, DTLS effectively adds a connection
layer -- state must be kept in the server for each "connected" peer.
Using UDP in Tokio to implement a stateful server seems to require a bit
of extra plumbing at this time.  My solution was to implement a
`UdpServer` stream which manages the actual UDP socket, and dispatches
incoming datagrams to child streams which are handled individually by
each DTLS session.

To run the server, use the "server" command-line argument followed by
the host and port of the local socket address for binding:

```
cargo run -- server 127.0.0.1:9000
```

For more feedback on what the server is doing, you can turn on logging
via the `RUST_LOG` environment variable:

```
RUST_LOG="tokio_dtls_example=debug" cargo run -- server 127.0.0.1:9000
```

To run the client, use the "client" command-line argument followed by
the destination host and port separated by a colon:

```
cargo run -- client 127.0.0.1:9000
```

If the client has successfully connected to the server, you can type
lines of text and see them echoed back to you:

```
You say goodbye
Echo: You say goodbye
I say hello
Echo: I say hello
```

When you close the input (e.g. by typing Ctrl-D on Mac or Linux), the
DTLS session should be properly shut down by each side issuing a TLS
`close_notify` alert.

If you use a tool like [Wireshark](https://www.wireshark.org/), you
should be able to see that communication is encrypted in TLS records
rather than being sent in plaintext:

![](https://raw.githubusercontent.com/simmons/tokio-dtls-example/master/dtls-wireshark.png)

To verify operation independently of this program, you can use the
OpenSSL command-line tool to connect to the server:

```
openssl s_client -dtls1 -connect 127.0.0.1:9000
```

As far as I can tell, this is a functioning example of DTLS.  Please let
me know if you discover any faults.

License
----------------------------------------

This crate is distributed under the terms of both the MIT license and
the Apache License (Version 2.0).  See LICENSE-MIT and LICENSE-APACHE
for details.

#### Contributing

Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion in the work, as defined in the
Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
