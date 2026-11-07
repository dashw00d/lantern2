//! Inode to same-user process resolution and caches.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::procinfo::{ProcKey, ProcReader};
use super::source::RawSocket;

#[derive(Debug, Clone)]
struct Owner {
    key: ProcKey,
    fd: PathBuf,
}
#[derive(Debug, Clone, Copy)]
struct Negative {
    attempts: u8,
    at: Instant,
}
#[derive(Debug, Default)]
pub(super) struct Owners {
    positive: HashMap<u64, Owner>,
    negative: HashMap<u64, Negative>,
    unreadable_fds: HashMap<ProcKey, Instant>,
}
impl Owners {
    fn matches(fd: &PathBuf, inode: u64) -> bool {
        fs::read_link(fd)
            .ok()
            .is_some_and(|p| p.to_string_lossy() == format!("socket:[{inode}]"))
    }
    fn find_fd(reader: &ProcReader, pid: u32, inode: u64) -> Option<PathBuf> {
        fs::read_dir(reader.root.pid_path(pid).join("fd"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|path| Self::matches(path, inode))
    }
    fn topmost(reader: &mut ProcReader, mut owner: Owner, inode: u64, now: Instant) -> Owner {
        if let Some(proc) = reader.read(owner.key.pid, now)
            && proc.key == owner.key
        {
            for ancestor in proc.ancestors {
                if ancestor.uid != reader.root.uid() {
                    break;
                }
                if let Some(fd) = Self::find_fd(reader, ancestor.key.pid, inode) {
                    owner = Owner {
                        key: ancestor.key,
                        fd,
                    };
                }
            }
        }
        owner
    }
    pub fn resolve(
        &mut self,
        sockets: &[RawSocket],
        reader: &mut ProcReader,
        now: Instant,
    ) -> HashMap<u64, ProcKey> {
        self.unreadable_fds
            .retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(60));
        let wanted_all: HashSet<_> = sockets
            .iter()
            .filter(|s| s.uid == reader.root.uid())
            .map(|s| s.inode)
            .collect();
        self.positive.retain(|inode, o| {
            wanted_all.contains(inode)
                && reader.key(o.key.pid).ok() == Some(o.key)
                && Self::matches(&o.fd, *inode)
        });
        self.negative.retain(|inode, _| wanted_all.contains(inode));
        let mut wanted: HashSet<_> = wanted_all
            .iter()
            .copied()
            .filter(|inode| {
                !self.positive.contains_key(inode)
                    && self.negative.get(inode).is_none_or(|n| {
                        n.attempts < 2 || now.saturating_duration_since(n.at) >= Duration::from_secs(60)
                    })
            })
            .collect();
        if !wanted.is_empty()
            && let Ok(dir) = fs::read_dir(reader.root.path())
        {
            let mut pids: Vec<u32> = dir
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect();
            pids.sort_unstable_by(|a, b| b.cmp(a));
            for pid in pids {
                if wanted.is_empty() {
                    break;
                }
                if reader.root.uid_of(pid).ok() != Some(reader.root.uid()) {
                    continue;
                }
                let Ok(key) = reader.key(pid) else {
                    continue;
                };
                if self.unreadable_fds.contains_key(&key) {
                    continue;
                }
                let fds = match fs::read_dir(reader.root.pid_path(pid).join("fd")) {
                    Ok(fds) => fds,
                    Err(err) => {
                        if err.kind() == std::io::ErrorKind::PermissionDenied {
                            self.unreadable_fds.insert(key, now);
                        }
                        continue;
                    }
                };
                for fd in fds.flatten() {
                    let path = fd.path();
                    let Some(inode) = fs::read_link(&path).ok().and_then(|p| {
                        let s = p.to_string_lossy();
                        s.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok()
                    }) else {
                        continue;
                    };
                    if wanted.contains(&inode) && reader.key(pid).ok() == Some(key) {
                        wanted.remove(&inode);
                        let owner = Self::topmost(reader, Owner { key, fd: path }, inode, now);
                        self.positive.insert(inode, owner);
                        self.negative.remove(&inode);
                    }
                    if wanted.is_empty() {
                        break;
                    }
                }
            }
        }
        for inode in wanted {
            let old = self.negative.get(&inode).map_or(0, |n| n.attempts);
            self.negative.insert(
                inode,
                Negative {
                    attempts: old.saturating_add(1),
                    at: now,
                },
            );
        }
        self.positive
            .iter()
            .map(|(inode, owner)| (*inode, owner.key))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::procinfo::{ProcRoot, fixture::Tree};
    #[test]
    fn discovery_negative_inode_backoff_and_retry() {
        let t = Tree::new();
        t.process(123, 1, 5, "node", "/repo", &["node"], "/app.service");
        let mut reader = ProcReader::new(ProcRoot::new(&t.0));
        let now = Instant::now();
        let socket = RawSocket {
            addr: "127.0.0.1:39202".parse().unwrap(),
            uid: reader.root.uid(),
            inode: 999,
        };
        let mut owners = Owners::default();
        assert!(
            owners
                .resolve(std::slice::from_ref(&socket), &mut reader, now)
                .is_empty()
        );
        assert!(
            owners
                .resolve(
                    std::slice::from_ref(&socket),
                    &mut reader,
                    now + Duration::from_secs(1)
                )
                .is_empty()
        );
        t.socket(123, 8, 999);
        assert!(
            owners
                .resolve(
                    std::slice::from_ref(&socket),
                    &mut reader,
                    now + Duration::from_secs(2)
                )
                .is_empty()
        );
        assert_eq!(
            owners
                .resolve(&[socket], &mut reader, now + Duration::from_secs(62))
                .get(&999),
            Some(&ProcKey {
                pid: 123,
                start_ticks: 5
            })
        );
    }
    #[test]
    fn discovery_fixture_owners_pid_reuse_and_ancestors() {
        let t = Tree::new();
        t.process(
            100,
            1,
            40,
            "parent",
            "/repo",
            &["parent"],
            "/user.slice/app.service",
        );
        t.process(
            200,
            100,
            50,
            "child",
            "/repo/sub",
            &["child"],
            "/user.slice/app.service",
        );
        t.socket(200, 4, 456);
        t.socket(100, 5, 456);
        let mut reader = ProcReader::new(ProcRoot::new(&t.0));
        let now = Instant::now();
        let socket = RawSocket {
            addr: "127.0.0.1:39200".parse().unwrap(),
            uid: reader.root.uid(),
            inode: 456,
        };
        let mut owners = Owners::default();
        assert_eq!(
            owners
                .resolve(std::slice::from_ref(&socket), &mut reader, now)
                .get(&456),
            Some(&ProcKey {
                pid: 100,
                start_ticks: 40
            })
        );
        let info = reader.read(200, now).unwrap();
        assert_eq!(info.ancestors[0].key.pid, 100);
        assert_eq!(info.exe.unwrap().to_string_lossy(), "/usr/bin/node");
        assert!(info.started_at.is_some());
        fs::remove_file(t.0.join("100/fd/5")).unwrap();
        t.process(
            100,
            1,
            99,
            "reused",
            "/different (deleted)",
            &["other"],
            "/user.slice/app.service",
        );
        assert_eq!(
            owners.resolve(&[socket], &mut reader, now).get(&456),
            Some(&ProcKey {
                pid: 200,
                start_ticks: 50
            })
        );
        assert!(reader.read(100, now).unwrap().cwd.is_none());
        assert_eq!(reader.read(100, now).unwrap().key.start_ticks, 99);
    }
}
