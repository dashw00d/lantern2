//! TCP listening socket enumeration via sock_diag and /proc/net/tcp.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use rustix::net::{self, AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType};

/// A kernel LISTEN socket and its owner uid and inode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawSocket {
    /// Local bind address.
    pub addr: SocketAddr,
    /// Kernel socket inode.
    pub inode: u64,
    /// Owning user id.
    pub uid: u32,
}

/// The source used for a socket scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketSourceKind {
    /// Linux netlink sock_diag.
    SockDiag,
    /// Linux procfs TCP tables.
    ProcNetTcp,
}

/// A source of TCP listening sockets.
pub trait SocketSource: Send {
    /// Enumerate IPv4 and IPv6 TCP LISTEN sockets.
    fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>>;
}

/// Linux netlink sock_diag socket source.
#[derive(Debug, Default)]
pub struct SockDiag;

fn request(family: u8, seq: u32) -> [u8; 72] {
    let mut bytes = [0; 72];
    bytes[0..4].copy_from_slice(&72u32.to_ne_bytes());
    bytes[4..6].copy_from_slice(&20u16.to_ne_bytes());
    bytes[6..8].copy_from_slice(&0x301u16.to_ne_bytes());
    bytes[8..12].copy_from_slice(&seq.to_ne_bytes());
    bytes[16] = family;
    bytes[17] = 6;
    bytes[20..24].copy_from_slice(&(1u32 << 10).to_ne_bytes());
    bytes
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("checked message length"),
    )
}

// Parse every aligned message in a datagram, returning true on NLMSG_DONE.
fn parse_diag(bytes: &[u8], seq: u32, out: &mut Vec<RawSocket>) -> io::Result<bool> {
    let mut offset = 0;
    let mut done = false;
    while offset < bytes.len() {
        if bytes.len() - offset < 16 {
            return Err(invalid("short netlink header"));
        }
        let msg = &bytes[offset..];
        let len = u32_at(msg, 0) as usize;
        if len < 16 || len > msg.len() {
            return Err(invalid("invalid netlink length"));
        }
        let kind = u16::from_ne_bytes([msg[4], msg[5]]);
        let flags = u16::from_ne_bytes([msg[6], msg[7]]);
        if u32_at(msg, 8) != seq {
            return Err(invalid("unexpected netlink sequence"));
        }
        if flags & 0x10 != 0 {
            return Err(invalid("interrupted netlink dump"));
        }
        match kind {
            3 => {
                if len >= 20 {
                    let err = i32::from_ne_bytes(msg[16..20].try_into().expect("checked length"));
                    if err < 0 {
                        return Err(io::Error::from_raw_os_error(-err));
                    }
                }
                done = true;
            }
            2 => {
                if len < 20 {
                    return Err(invalid("short netlink error"));
                }
                let err = i32::from_ne_bytes(msg[16..20].try_into().expect("checked length"));
                if err != 0 {
                    return Err(io::Error::from_raw_os_error(err.saturating_neg()));
                }
            }
            20 => {
                if len < 88 {
                    return Err(invalid("short inet_diag_msg"));
                }
                let data = &msg[16..len];
                if data[1] == 10 {
                    let ip = match data[0] {
                        2 => IpAddr::V4(Ipv4Addr::new(data[8], data[9], data[10], data[11])),
                        10 => IpAddr::V6(Ipv6Addr::from(
                            <[u8; 16]>::try_from(&data[8..24]).expect("checked length"),
                        )),
                        _ => return Err(invalid("unknown inet_diag family")),
                    };
                    out.push(RawSocket {
                        addr: SocketAddr::new(ip, u16::from_be_bytes([data[4], data[5]])),
                        uid: u32_at(data, 64),
                        inode: u64::from(u32_at(data, 68)),
                    });
                }
            }
            _ => {}
        }
        let aligned = (len + 3) & !3;
        if aligned > msg.len() {
            return Err(invalid("short netlink alignment"));
        }
        offset += aligned;
    }
    Ok(done)
}

impl SocketSource for SockDiag {
    fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>> {
        let fd = net::socket_with(
            AddressFamily::NETLINK,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            Some(net::netlink::SOCK_DIAG),
        )?;
        net::bind(&fd, &net::netlink::SocketAddrNetlink::new(0, 0))?;
        let kernel = net::netlink::SocketAddrNetlink::new(0, 0);
        let mut out = Vec::new();
        let mut buffer = vec![0; 65536];
        for (seq, family) in [(1, 2), (2, 10)] {
            net::sendto(&fd, &request(family, seq), SendFlags::empty(), &kernel)?;
            loop {
                let (read, full_len, _) = net::recvfrom(&fd, &mut buffer[..], RecvFlags::TRUNC)?;
                if full_len > buffer.len() {
                    return Err(invalid("truncated netlink datagram"));
                }
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "empty netlink datagram",
                    ));
                }
                if parse_diag(&buffer[..read], seq, &mut out)? {
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// Procfs fallback, rooted at an injectable proc directory.
#[derive(Debug, Clone)]
pub struct ProcNetTcp {
    root: PathBuf,
}
impl ProcNetTcp {
    /// Construct a source rooted at a proc filesystem or fixture.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}
impl Default for ProcNetTcp {
    fn default() -> Self {
        Self::new("/proc")
    }
}

fn parse_addr(hex: &str, v6: bool) -> io::Result<IpAddr> {
    let words = if v6 { 4 } else { 1 };
    if hex.len() != words * 8 || !hex.is_ascii() {
        return Err(invalid("invalid proc address"));
    }
    let mut bytes = [0u8; 16];
    for i in 0..words {
        let word =
            u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).map_err(|_| invalid("invalid proc address"))?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_ne_bytes());
    }
    Ok(if v6 {
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))
    })
}

fn parse_table(text: &str, v6: bool, out: &mut Vec<RawSocket>, seen: &mut HashSet<u64>) -> io::Result<()> {
    for line in text.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return Err(invalid("short proc TCP row"));
        }
        if fields[3] != "0A" {
            continue;
        }
        let (ip, port) = fields[1]
            .split_once(':')
            .ok_or_else(|| invalid("invalid proc socket"))?;
        let addr = SocketAddr::new(
            parse_addr(ip, v6)?,
            u16::from_str_radix(port, 16).map_err(|_| invalid("invalid proc port"))?,
        );
        let uid = fields[7].parse().map_err(|_| invalid("invalid proc uid"))?;
        let inode = fields[9].parse().map_err(|_| invalid("invalid proc inode"))?;
        if seen.insert(inode) {
            out.push(RawSocket { addr, uid, inode });
        }
    }
    Ok(())
}
impl SocketSource for ProcNetTcp {
    fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (name, v6) in [("tcp", false), ("tcp6", true)] {
            let path = self.root.join(Path::new("net")).join(name);
            match fs::read_to_string(path) {
                Ok(data) => parse_table(&data, v6, &mut out, &mut seen)?,
                Err(e) if v6 && e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// Prefer sock_diag, falling back to procfs when the kernel query fails.
#[derive(Debug)]
pub struct AutoSource {
    proc: ProcNetTcp,
    warned: bool,
}
impl AutoSource {
    /// Create a source with an injectable procfs fallback root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            proc: ProcNetTcp::new(root),
            warned: false,
        }
    }
    /// Return both the sockets and the source that succeeded.
    pub fn scan(&mut self) -> io::Result<(SocketSourceKind, Vec<RawSocket>)> {
        self.scan_with(&mut SockDiag)
    }
    fn scan_with(
        &mut self,
        primary: &mut impl SocketSource,
    ) -> io::Result<(SocketSourceKind, Vec<RawSocket>)> {
        match primary.listening_tcp() {
            Ok(rows) => Ok((SocketSourceKind::SockDiag, rows)),
            Err(e) => {
                if !self.warned {
                    eprintln!("lantern discovery: sock_diag unavailable ({e}); falling back to procfs");
                    self.warned = true;
                }
                Ok((SocketSourceKind::ProcNetTcp, self.proc.listening_tcp()?))
            }
        }
    }
}
impl Default for AutoSource {
    fn default() -> Self {
        Self::new("/proc")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_proc_fixture_and_automatic_fallback() {
        use crate::discovery::procinfo::fixture::Tree;
        #[derive(Debug)]
        struct Failed;
        impl SocketSource for Failed {
            fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
        let t = Tree::new();
        std::fs::create_dir_all(t.0.join("net")).unwrap();
        let header = "sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode\n";
        std::fs::write(
            t.0.join("net/tcp"),
            format!("{header}0: 0100007F:9920 00000000:0000 0A 0:0 0:0 0 1000 0 456\n"),
        )
        .unwrap();
        std::fs::write(t.0.join("net/tcp6"), format!("{header}0: 00000000000000000000000001000000:9921 00000000000000000000000000000000:0000 0A 0:0 0:0 0 1000 0 457\n")).unwrap();
        let mut auto = AutoSource::new(&t.0);
        let (source, rows) = auto.scan_with(&mut Failed).unwrap();
        assert_eq!(source, SocketSourceKind::ProcNetTcp);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].addr, "127.0.0.1:39200".parse().unwrap());
        assert_eq!(rows[1].addr, "[::1]:39201".parse().unwrap());
        assert_eq!(
            parse_addr("0000000000000000FFFF00000100007F", true).unwrap(),
            "::ffff:127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }
    #[test]
    fn discovery_diag_error_and_ipv6() {
        let mut msg = vec![0; 88];
        msg[..4].copy_from_slice(&88u32.to_ne_bytes());
        msg[4..6].copy_from_slice(&20u16.to_ne_bytes());
        msg[8..12].copy_from_slice(&7u32.to_ne_bytes());
        msg[16] = 10;
        msg[17] = 10;
        msg[20..22].copy_from_slice(&39200u16.to_be_bytes());
        msg[39] = 1;
        msg[80..84].copy_from_slice(&1000u32.to_ne_bytes());
        msg[84..88].copy_from_slice(&1234u32.to_ne_bytes());
        let mut out = Vec::new();
        assert!(!parse_diag(&msg, 7, &mut out).unwrap());
        assert_eq!(out[0].addr, "[::1]:39200".parse().unwrap());
        let mut err = [0u8; 20];
        err[..4].copy_from_slice(&20u32.to_ne_bytes());
        err[4..6].copy_from_slice(&2u16.to_ne_bytes());
        err[8..12].copy_from_slice(&7u32.to_ne_bytes());
        err[16..20].copy_from_slice(&(-13i32).to_ne_bytes());
        assert_eq!(
            parse_diag(&err, 7, &mut Vec::new()).unwrap_err().raw_os_error(),
            Some(13)
        );
        assert!(parse_diag(&msg, 8, &mut Vec::new()).is_err());
    }
    #[test]
    fn discovery_proc_tcp_parse() {
        let head = "sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode\n";
        let data = format!(
            "{head}0: 0100007F:0F5C 00000000:0000 0A 0:0 0:0 0 1000 0 42\n1: 00000000:0000 00000000:0000 01 0:0 0:0 0 0 0 99\n2: 0100007F:0F5C 00000000:0000 0A 0:0 0:0 0 1000 0 42\n"
        );
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        parse_table(&data, false, &mut out, &mut seen).unwrap();
        assert_eq!(
            out,
            vec![RawSocket {
                addr: "127.0.0.1:3932".parse().unwrap(),
                uid: 1000,
                inode: 42
            }]
        );
        assert_eq!(
            parse_addr("00000000000000000000000001000000", true).unwrap(),
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
        assert_eq!(
            parse_addr("00000000000000000000000000000000", true).unwrap(),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        );
        assert!(parse_addr("xxx", false).is_err());
    }
    #[test]
    fn discovery_diag_request_and_messages() {
        let req = request(2, 42);
        assert_eq!(&req[0..4], &72u32.to_ne_bytes());
        assert_eq!(&req[20..24], &(1u32 << 10).to_ne_bytes());
        let mut msg = vec![0; 88];
        msg[..4].copy_from_slice(&88u32.to_ne_bytes());
        msg[4..6].copy_from_slice(&20u16.to_ne_bytes());
        msg[8..12].copy_from_slice(&42u32.to_ne_bytes());
        msg[16] = 2;
        msg[17] = 10;
        msg[20..22].copy_from_slice(&8080u16.to_be_bytes());
        msg[24..28].copy_from_slice(&[127, 0, 0, 1]);
        msg[80..84].copy_from_slice(&1000u32.to_ne_bytes());
        msg[84..88].copy_from_slice(&123u32.to_ne_bytes());
        let mut done = vec![0; 16];
        done[..4].copy_from_slice(&16u32.to_ne_bytes());
        done[4..6].copy_from_slice(&3u16.to_ne_bytes());
        done[8..12].copy_from_slice(&42u32.to_ne_bytes());
        msg.extend(done);
        let mut out = Vec::new();
        assert!(parse_diag(&msg, 42, &mut out).unwrap());
        assert_eq!(out[0].addr, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(out[0].uid, 1000);
        assert_eq!(out[0].inode, 123);
        assert!(parse_diag(&msg[..20], 42, &mut Vec::new()).is_err());
    }
}
