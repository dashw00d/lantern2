//! Minimal authoritative DNS responder for the configured local TLD.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

use super::{caddy::NetError, routes::valid_host};

/// Loopback listener and DNS response policy.
#[derive(Debug, Clone)]
pub struct DnsConfig {
    /// IPv4 loopback address and port for both UDP and TCP.
    pub listen: SocketAddrV4,
    /// Single-label local TLD.
    pub tld: String,
    /// Answer AAAA queries with ::1 when true.
    pub ipv6: bool,
}

/// DNS server namespace.
#[derive(Debug)]
pub struct Dns;

fn word(packet: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *packet.get(offset)?,
        *packet.get(offset + 1)?,
    ]))
}
fn push_word(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_be_bytes());
}
fn header(query: &[u8], rcode: u16, question: bool, answers: u16, opt: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(query.len().min(512) + 32);
    out.extend_from_slice(&query[..2]);
    let flags = word(query, 2).unwrap_or(0);
    push_word(&mut out, 0x8400 | (flags & 0x7900) | rcode);
    push_word(&mut out, u16::from(question));
    push_word(&mut out, answers);
    push_word(&mut out, 0);
    push_word(&mut out, u16::from(opt));
    out
}
fn malformed(query: &[u8], code: u16) -> Option<Vec<u8>> {
    Some(header(query, code, false, 0, false))
}

// Question names must be uncompressed; RR names may be pointers, without following them.
fn skip_name(packet: &[u8], mut pos: usize, allow_pointer: bool) -> Option<(usize, Vec<String>)> {
    let start = pos;
    let mut labels = Vec::new();
    loop {
        let len = *packet.get(pos)?;
        pos += 1;
        if len == 0 {
            return Some((pos, labels));
        }
        if len & 0xc0 == 0xc0 && allow_pointer {
            let next = *packet.get(pos)?;
            let target = (usize::from(len & 0x3f) << 8) | usize::from(next);
            if target >= packet.len() {
                return None;
            }
            return Some((pos + 1, labels));
        }
        if len & 0xc0 != 0 || len > 63 || pos.checked_add(usize::from(len))? > packet.len() {
            return None;
        }
        let bytes = &packet[pos..pos + usize::from(len)];
        labels.push(String::from_utf8_lossy(bytes).to_ascii_lowercase());
        pos += usize::from(len);
        if pos - start > 254 {
            return None;
        }
    }
}

/// Decode a query and return a wire-format reply, or drop non-query/truncated headers.
pub fn respond(query: &[u8], tld: &str, ipv6: bool) -> Option<Vec<u8>> {
    if query.len() < 12 || word(query, 2)? & 0x8000 != 0 {
        return None;
    }
    let flags = word(query, 2)?;
    if flags & 0x7800 != 0 {
        return malformed(query, 4);
    }
    if word(query, 4)? != 1 {
        return malformed(query, 1);
    }
    let Some((end_name, labels)) = skip_name(query, 12, false) else {
        return malformed(query, 1);
    };
    let Some(qtype) = word(query, end_name) else {
        return malformed(query, 1);
    };
    let Some(qclass) = word(query, end_name + 2) else {
        return malformed(query, 1);
    };
    let question_end = end_name + 4;
    let mut offset = question_end;
    let mut opt = false;
    for section in 0..3 {
        let count = word(query, 6 + 2 * section)?;
        for _ in 0..count {
            let Some((end, _)) = skip_name(query, offset, true) else {
                return malformed(query, 1);
            };
            let Some(typ) = word(query, end) else {
                return malformed(query, 1);
            };
            let Some(len) = word(query, end + 8) else {
                return malformed(query, 1);
            };
            let Some(next) = end.checked_add(10 + usize::from(len)) else {
                return malformed(query, 1);
            };
            if next > query.len() {
                return malformed(query, 1);
            }
            if section == 2 && typ == 41 {
                opt = true;
            }
            offset = next;
        }
    }
    if offset != query.len() {
        return malformed(query, 1);
    }
    let in_zone = labels.last().is_some_and(|last| last.eq_ignore_ascii_case(tld));
    let code = if !matches!(qclass, 1 | 255) || !in_zone {
        5
    } else {
        0
    };
    let answer = code == 0 && labels.len() > 1 && (qtype == 1 || (qtype == 28 && ipv6));
    let mut out = header(query, code, true, u16::from(answer), opt);
    out.extend_from_slice(&query[12..question_end]);
    if answer {
        push_word(&mut out, 0xc00c);
        push_word(&mut out, qtype);
        push_word(&mut out, 1);
        out.extend_from_slice(&60u32.to_be_bytes());
        if qtype == 1 {
            push_word(&mut out, 4);
            out.extend_from_slice(&[127, 0, 0, 1]);
        } else {
            push_word(&mut out, 16);
            out.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        }
    }
    if opt {
        out.push(0);
        push_word(&mut out, 41);
        push_word(&mut out, 1232);
        out.extend_from_slice(&[0; 6]);
    }
    Some(out)
}

async fn tcp_client(mut stream: TcpStream, cfg: DnsConfig, cancel: CancellationToken) {
    loop {
        let operation = async {
            let len = stream.read_u16().await? as usize;
            if len == 0 {
                return Err(std::io::Error::other("empty DNS frame"));
            }
            let mut packet = vec![0; len];
            stream.read_exact(&mut packet).await?;
            if let Some(reply) = respond(&packet, &cfg.tld, cfg.ipv6) {
                stream
                    .write_u16(
                        u16::try_from(reply.len())
                            .map_err(|_| std::io::Error::other("DNS reply too large"))?,
                    )
                    .await?;
                stream.write_all(&reply).await?;
            }
            Ok::<(), std::io::Error>(())
        };
        tokio::select! { _ = cancel.cancelled() => return, r = tokio::time::timeout(Duration::from_secs(5), operation) => if !matches!(r, Ok(Ok(()))) { return; } }
    }
}

impl Dns {
    /// Serve UDP and TCP on IPv4 loopback until cancellation.
    pub async fn serve(cfg: DnsConfig, cancel: CancellationToken) -> Result<(), NetError> {
        if *cfg.listen.ip() != Ipv4Addr::LOCALHOST || !valid_host(&cfg.tld) || cfg.tld.contains('.') {
            return Err(NetError::Invalid(
                "DNS requires 127.0.0.1 and a valid single-label TLD".into(),
            ));
        }
        let udp = UdpSocket::bind(cfg.listen).await?;
        let addr = udp.local_addr()?;
        let tcp = TcpListener::bind(addr).await?;
        let sem = Arc::new(Semaphore::new(64));
        let mut packet = vec![0u8; 65535];
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                result = udp.recv_from(&mut packet) => {
                    let (n, peer) = result?;
                    if let Some(reply) = respond(&packet[..n], &cfg.tld, cfg.ipv6) { udp.send_to(&reply, peer).await?; }
                }
                result = tcp.accept() => {
                    let (stream, peer) = result?;
                    if !matches!(peer, SocketAddr::V4(v4) if v4.ip().is_loopback()) { continue; }
                    if let Ok(permit) = sem.clone().try_acquire_owned() {
                        let cfg = cfg.clone(); let cancel = cancel.clone();
                        tokio::spawn(async move { let _permit = permit; tcp_client(stream, cfg, cancel).await; });
                    }
                }
            }
        }
    }
}
