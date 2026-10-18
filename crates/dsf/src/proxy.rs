//! The egress proxy: the only way out of the agents' network. It runs in
//! a sidecar container attached to both that internal network and the
//! default bridge (`dsf proxy`), and allows `CONNECT` to allowlisted host
//! names only. TLS passes through untouched; dsf never sees the traffic.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_HEAD_BYTES: usize = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What the proxy lets through.
#[derive(Debug, Clone)]
pub struct Policy {
    hosts: BTreeSet<String>,
    ports: BTreeSet<u16>,
    /// Only for tests: production refuses names that resolve to private,
    /// loopback or link-local addresses, so an allowlisted name can't be
    /// pointed at the LAN.
    allow_private_addresses: bool,
}

impl Policy {
    /// HTTPS to these host names only.
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            hosts: hosts
                .into_iter()
                .map(|host| host.as_ref().to_ascii_lowercase())
                .collect(),
            ports: BTreeSet::from([443]),
            allow_private_addresses: false,
        }
    }

    /// Allows these ports instead of 443.
    #[must_use]
    pub fn with_ports(mut self, ports: &[u16]) -> Self {
        self.ports = ports.iter().copied().collect();
        self
    }

    /// Lets allowlisted names resolve to private addresses. Only for tests
    /// against local servers.
    #[must_use]
    pub fn allowing_private_addresses(mut self) -> Self {
        self.allow_private_addresses = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { host: String, port: u16 },
    Deny(String),
}

/// Judges a request head by its request line alone.
pub fn decide(head: &str, policy: &Policy) -> Decision {
    let line = head.lines().next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Decision::Deny("malformed request".to_owned());
    };
    if method != "CONNECT" {
        return Decision::Deny(format!("{method} is not allowed; only CONNECT"));
    }
    if !version.starts_with("HTTP/1.") {
        return Decision::Deny("unsupported HTTP version".to_owned());
    }
    let Some((host, port)) = target.rsplit_once(':') else {
        return Decision::Deny(format!("`{target}` has no port"));
    };
    let host = host.to_ascii_lowercase();
    let Ok(port) = port.parse::<u16>() else {
        return Decision::Deny(format!("`{target}` has a bad port"));
    };
    if !policy.hosts.contains(&host) {
        return Decision::Deny(format!("{host} is not allowlisted"));
    }
    if !policy.ports.contains(&port) {
        return Decision::Deny(format!("port {port} is not allowed"));
    }
    Decision::Allow { host, port }
}

/// Addresses a public service could legitimately have.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        // Shared address space, which Tailscale uses.
        || (a == 100 && (64..=127).contains(&b))
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local fc00::/7 and link-local fe80::/10.
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80)
}

/// Accepts connections until the listener fails.
pub async fn serve(listener: TcpListener, policy: Policy) -> Result<()> {
    let policy = Arc::new(policy);
    loop {
        let (client, peer) = listener.accept().await?;
        let policy = Arc::clone(&policy);
        tokio::spawn(async move {
            if let Err(error) = handle(client, &policy).await {
                println!("error {peer}: {error:#}");
            }
        });
    }
}

async fn handle(mut client: TcpStream, policy: &Policy) -> Result<()> {
    let (head, rest) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await??;
    let (host, port) = match decide(&head, policy) {
        Decision::Allow { host, port } => (host, port),
        Decision::Deny(reason) => {
            println!("deny  {reason}");
            client
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Ok(());
        }
    };
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .filter(|address| policy.allow_private_addresses || is_public(address.ip()))
        .collect();
    if addresses.is_empty() {
        println!("deny  {host}:{port} resolves to no public address");
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }
    let Some(mut upstream) = connect_any(&addresses).await else {
        println!("fail  {host}:{port} unreachable");
        client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    };
    println!("allow {host}:{port}");
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    // Bytes the client sent after the head, e.g. an eager TLS hello.
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Tries each address in turn. Docker networks often have no IPv6 route
/// even when a name has IPv6 addresses, so the first may well fail.
async fn connect_any(addresses: &[SocketAddr]) -> Option<TcpStream> {
    for address in addresses {
        if let Ok(Ok(stream)) =
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await
        {
            return Some(stream);
        }
    }
    None
}

/// Reads up to the blank line ending the request head, returning the head
/// and any bytes read past it.
async fn read_head(client: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = client.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "client closed before sending a request");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buffer).into_owned(), rest));
        }
        anyhow::ensure!(buffer.len() <= MAX_HEAD_BYTES, "request head too large");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(["api.anthropic.com"])
    }

    #[test]
    fn allows_connect_to_allowlisted_hosts_on_443() {
        assert_eq!(
            decide(
                "CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: x\r\n\r\n",
                &policy()
            ),
            Decision::Allow {
                host: "api.anthropic.com".into(),
                port: 443
            }
        );
        assert_eq!(
            decide("CONNECT API.Anthropic.COM:443 HTTP/1.1", &policy()),
            Decision::Allow {
                host: "api.anthropic.com".into(),
                port: 443
            }
        );
    }

    #[test]
    fn denies_everything_else() {
        for head in [
            "CONNECT example.com:443 HTTP/1.1",
            "CONNECT api.anthropic.com:22 HTTP/1.1",
            "CONNECT api.anthropic.com HTTP/1.1",
            "CONNECT api.anthropic.com:443x HTTP/1.1",
            "CONNECT [::1]:443 HTTP/1.1",
            "CONNECT 160.79.104.10:443 HTTP/1.1",
            "CONNECT api.anthropic.com.evil.test:443 HTTP/1.1",
            "GET http://api.anthropic.com/ HTTP/1.1",
            "CONNECT api.anthropic.com:443 HTTP/2",
            "CONNECT api.anthropic.com:443 HTTP/1.1 extra",
            "",
        ] {
            assert!(
                matches!(decide(head, &policy()), Decision::Deny(_)),
                "{head:?}"
            );
        }
    }

    #[test]
    fn only_public_addresses_count() {
        for public in [
            "160.79.104.10",
            "8.8.8.8",
            "2607:6bc0::10",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        for private in [
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.249",
            "127.0.0.1",
            "169.254.169.254",
            "100.100.100.100",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
    }
}
