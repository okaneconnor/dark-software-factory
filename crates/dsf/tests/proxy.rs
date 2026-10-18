//! The egress proxy over real sockets: an allowed tunnel carries bytes both
//! ways, and everything else is refused before any connection is made.

// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

use dsf::proxy::{Policy, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An upstream that echoes whatever it receives.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                while let Ok(read) = socket.read(&mut buffer).await {
                    if read == 0 || socket.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

async fn start_proxy(policy: Policy) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, policy));
    port
}

/// Sends a request head plus `eager` bytes, returning the status line.
async fn connect(proxy: u16, target: &str, eager: &[u8]) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").into_bytes();
    request.extend_from_slice(eager);
    stream.write_all(&request).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8(head)
        .unwrap()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    (status, stream)
}

#[tokio::test]
async fn an_allowed_tunnel_carries_bytes_both_ways() {
    let upstream = echo_server().await;
    let proxy = start_proxy(
        Policy::new(["localhost"])
            .with_ports(&[upstream])
            .allowing_private_addresses(),
    )
    .await;
    // Bytes sent before the 200 arrives must reach the upstream too.
    let (status, mut stream) = connect(proxy, &format!("localhost:{upstream}"), b"early ").await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    stream.write_all(b"ping").await.unwrap();
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"early ping");
}

#[tokio::test]
async fn refuses_hosts_ports_and_methods_outside_the_policy() {
    let upstream = echo_server().await;
    let proxy = start_proxy(
        Policy::new(["localhost"])
            .with_ports(&[upstream])
            .allowing_private_addresses(),
    )
    .await;
    for target in [
        "example.com:443",
        &format!("localhost:{}", upstream + 1),
        &format!("127.0.0.1:{upstream}"),
    ] {
        let (status, _) = connect(proxy, target, b"").await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
    }
    let mut stream = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
    stream
        .write_all(b"GET http://localhost/ HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 403 Forbidden"), "{reply}");
}

#[tokio::test]
async fn refuses_allowlisted_names_that_resolve_to_private_addresses() {
    let upstream = echo_server().await;
    // As in production, except for the port: localhost is allowlisted but
    // resolves to loopback, so it must still be refused.
    let proxy = start_proxy(Policy::new(["localhost"]).with_ports(&[upstream])).await;
    let (status, _) = connect(proxy, &format!("localhost:{upstream}"), b"").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
}
