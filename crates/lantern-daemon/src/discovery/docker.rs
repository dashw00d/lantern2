//! Best-effort docker-proxy labels without using the Docker API.
use super::procinfo::ProcRoot;
use super::source::RawSocket;
use lantern_core::model::DockerInfo;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::net::IpAddr;

#[derive(Debug)]
struct Proxy {
    host_port: u16,
    host_ip: Option<IpAddr>,
    container_port: Option<u16>,
}
fn parse_proxy(bytes: &[u8]) -> Option<Proxy> {
    let args: Vec<_> = bytes
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(String::from_utf8_lossy)
        .collect();
    let mut proto = None;
    let mut host_port = None;
    let mut host_ip = None;
    let mut container_port = None;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_ref();
        let (name, value, consumed) = if let Some((name, value)) = arg.split_once('=') {
            (name, Some(value), 1)
        } else {
            (arg, args.get(i + 1).map(|v| v.as_ref()), 2)
        };
        match name {
            "-proto" => proto = value,
            "-host-port" => host_port = value.and_then(|v| v.parse().ok()),
            "-host-ip" => host_ip = Some(value?.parse().ok()?),
            "-container-port" => container_port = value.and_then(|v| v.parse().ok()),
            _ => {
                i += 1;
                continue;
            }
        }
        i += consumed;
    }
    if proto != Some("tcp") {
        return None;
    }
    Some(Proxy {
        host_port: host_port?,
        host_ip,
        container_port,
    })
}
#[derive(Debug, Default)]
pub(super) struct DockerCache {
    by_inode: HashMap<u64, Option<DockerInfo>>,
}
impl DockerCache {
    pub fn enrich(&mut self, sockets: &[RawSocket], root: &ProcRoot) -> HashMap<u64, DockerInfo> {
        let foreign: Vec<_> = sockets.iter().filter(|s| s.uid != root.uid()).collect();
        let live: HashSet<_> = foreign.iter().map(|s| s.inode).collect();
        self.by_inode.retain(|inode, _| live.contains(inode));
        if foreign.iter().any(|s| !self.by_inode.contains_key(&s.inode)) {
            let mut proxies = Vec::new();
            if let Ok(entries) = fs::read_dir(root.path()) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                        continue;
                    }
                    let path = entry.path();
                    if fs::read_to_string(path.join("comm"))
                        .ok()
                        .as_deref()
                        .map(str::trim)
                        != Some("docker-proxy")
                    {
                        continue;
                    }
                    let mut bytes = Vec::new();
                    if let Ok(file) = File::open(path.join("cmdline"))
                        && file.take(16 * 1024).read_to_end(&mut bytes).is_ok()
                        && let Some(proxy) = parse_proxy(&bytes)
                    {
                        proxies.push(proxy);
                    }
                }
            }
            for socket in foreign {
                self.by_inode.entry(socket.inode).or_insert_with(|| {
                    proxies
                        .iter()
                        .find(|p| {
                            p.host_port == socket.addr.port()
                                && p.host_ip.is_none_or(|ip| ip == socket.addr.ip())
                        })
                        .map(|p| DockerInfo {
                            label: "Docker".into(),
                            container_port: p.container_port,
                        })
                });
            }
        }
        self.by_inode
            .iter()
            .filter_map(|(inode, info)| info.clone().map(|info| (*inode, info)))
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::procinfo::fixture::Tree;
    #[test]
    fn discovery_docker_proxy_fixture() {
        let tree = Tree::new();
        tree.process(
            123,
            1,
            5,
            "docker-proxy",
            "/",
            &[
                "docker-proxy",
                "-proto",
                "tcp",
                "-host-ip",
                "0.0.0.0",
                "-host-port",
                "39200",
                "-container-ip",
                "172.17.0.2",
                "-container-port",
                "80",
            ],
            "/system.slice/docker.service",
        );
        fs::write(tree.0.join("123/comm"), "docker-proxy\n").unwrap();
        let root = ProcRoot::new(&tree.0);
        let uid = root.uid();
        let sockets = vec![
            RawSocket {
                addr: "0.0.0.0:39200".parse().unwrap(),
                inode: 8,
                uid: uid + 1,
            },
            RawSocket {
                addr: "127.0.0.1:39201".parse().unwrap(),
                inode: 9,
                uid: uid + 1,
            },
        ];
        let result = DockerCache::default().enrich(&sockets, &root);
        assert_eq!(result.get(&8).unwrap().container_port, Some(80));
        assert!(!result.contains_key(&9));
        assert!(parse_proxy(b"docker-proxy\0-proto\0udp\0-host-port\x0039200\0").is_none());
        assert!(
            parse_proxy(b"docker-proxy\0-proto\0tcp\0-host-ip\0invalid\0-host-port\x0039200\0").is_none()
        );
    }
}
