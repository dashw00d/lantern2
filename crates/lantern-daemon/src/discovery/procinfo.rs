//! Fixture-rooted procfs process metadata and process identity cache.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lantern_core::Timestamp;

/// Root of a proc filesystem; can be replaced by a fixture tree.
#[derive(Debug, Clone)]
pub struct ProcRoot {
    root: PathBuf,
    uid: u32,
    ticks: u64,
}
impl ProcRoot {
    /// Construct a root using the current user's uid and kernel clock rate.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            root: path.into(),
            uid: rustix::process::getuid().as_raw(),
            ticks: rustix::param::clock_ticks_per_second(),
        }
    }
    /// The procfs root directory.
    pub fn path(&self) -> &Path {
        &self.root
    }
    /// The uid whose processes may be inspected.
    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub(super) fn pid_path(&self, pid: u32) -> PathBuf {
        self.root.join(pid.to_string())
    }
    pub(super) fn uid_of(&self, pid: u32) -> io::Result<u32> {
        Ok(fs::metadata(self.pid_path(pid))?.uid())
    }
    fn stat(&self, pid: u32) -> io::Result<Stat> {
        parse_stat(&fs::read_to_string(self.pid_path(pid).join("stat"))?)
    }
    fn boot_time(&self) -> Option<i64> {
        fs::read_to_string(self.root.join("stat"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok())
    }
    fn started_at(&self, ticks: u64) -> Option<Timestamp> {
        if self.ticks == 0 {
            return None;
        }
        let secs = i64::try_from(ticks / self.ticks)
            .ok()?
            .checked_add(self.boot_time()?)?;
        let nanos = ((ticks % self.ticks) as u128 * 1_000_000_000 / self.ticks as u128) as i32;
        Timestamp::new(secs, nanos).ok()
    }
}
impl Default for ProcRoot {
    fn default() -> Self {
        Self::new("/proc")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ProcKey {
    pub pid: u32,
    pub start_ticks: u64,
}
#[derive(Debug, Clone)]
pub(super) struct ProcInfo {
    pub key: ProcKey,
    pub ppid: u32,
    pub uid: u32,
    pub comm: String,
    pub exe: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub cmdline_raw: Vec<String>,
    pub unit: Option<String>,
    pub started_at: Option<Timestamp>,
    pub ancestors: Vec<ProcInfo>,
}
#[derive(Debug)]
struct Stat {
    comm: String,
    ppid: u32,
    start_ticks: u64,
}
fn parse_stat(input: &str) -> io::Result<Stat> {
    let start = input
        .find('(')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing stat comm"))?;
    let end = input
        .rfind(')')
        .filter(|end| *end > start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing stat comm end"))?;
    let fields: Vec<_> = input[end + 1..].split_whitespace().collect();
    if fields.len() < 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short stat"));
    }
    Ok(Stat {
        comm: input[start + 1..end].to_owned(),
        ppid: fields[1]
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid ppid"))?,
        start_ticks: fields[19]
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid starttime"))?,
    })
}
fn read_cmdline(path: &Path) -> Vec<String> {
    let mut bytes = Vec::new();
    if let Ok(file) = File::open(path) {
        let _ = file.take(16 * 1024).read_to_end(&mut bytes);
    }
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}
fn read_link(path: &Path, deleted_is_none: bool) -> Option<PathBuf> {
    let value = fs::read_link(path).ok()?;
    let text = value.to_string_lossy();
    if let Some(trimmed) = text.strip_suffix(" (deleted)") {
        if deleted_is_none {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    } else {
        Some(value)
    }
}
fn unescape_unit(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && i + 3 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4])
            && let Ok(ch) = u8::from_str_radix(hex, 16)
        {
            out.push(ch);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
/// Decode systemd's escaped unit name for comparisons.
pub fn normalize_unit(value: &str) -> String {
    unescape_unit(value)
}
/// Find the Lantern unit or innermost service in a unified cgroup v2 file.
pub fn unit_of(cgroup: &str) -> Option<String> {
    let mut lantern = None;
    let mut service = None;
    for line in cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        if fields.next() != Some("0") || fields.next() != Some("") {
            continue;
        }
        for part in fields.next()?.split('/').filter(|s| !s.is_empty()) {
            let unit = unescape_unit(part);
            if unit.starts_with("lantern") && (unit.ends_with(".service") || unit.ends_with(".scope")) {
                lantern = Some(unit.clone());
            }
            if unit.ends_with(".service") {
                service = Some(unit);
            }
        }
    }
    lantern.or(service)
}
/// Whether a unit name represents a Lantern-managed instance.
pub fn is_lantern_inst(unit: &str) -> bool {
    let unit = normalize_unit(unit);
    unit.starts_with("lantern-inst-") && (unit.ends_with(".service") || unit.ends_with(".scope"))
}

#[derive(Debug)]
struct Cached {
    info: ProcInfo,
    refreshed: Instant,
    last_used: Instant,
}
#[derive(Debug)]
pub(super) struct ProcReader {
    pub root: ProcRoot,
    cache: HashMap<ProcKey, Cached>,
}
impl ProcReader {
    pub fn new(root: ProcRoot) -> Self {
        Self {
            root,
            cache: HashMap::new(),
        }
    }
    pub fn key(&self, pid: u32) -> io::Result<ProcKey> {
        Ok(ProcKey {
            pid,
            start_ticks: self.root.stat(pid)?.start_ticks,
        })
    }
    fn one(&mut self, pid: u32, now: Instant) -> Option<ProcInfo> {
        let stat = self.root.stat(pid).ok()?;
        let key = ProcKey {
            pid,
            start_ticks: stat.start_ticks,
        };
        self.cache.retain(|k, c| {
            (k.pid != pid || *k == key)
                && now.saturating_duration_since(c.last_used) < Duration::from_secs(60)
        });
        if let Some(c) = self.cache.get_mut(&key)
            && now.saturating_duration_since(c.refreshed) < Duration::from_secs(30)
        {
            c.last_used = now;
            return Some(c.info.clone());
        }
        let path = self.root.pid_path(pid);
        let info = ProcInfo {
            key,
            ppid: stat.ppid,
            uid: self.root.uid_of(pid).ok()?,
            comm: stat.comm,
            exe: read_link(&path.join("exe"), false),
            cwd: read_link(&path.join("cwd"), true),
            cmdline_raw: read_cmdline(&path.join("cmdline")),
            unit: fs::read_to_string(path.join("cgroup"))
                .ok()
                .and_then(|s| unit_of(&s)),
            started_at: self.root.started_at(key.start_ticks),
            ancestors: Vec::new(),
        };
        // The pid may have been reused while its attributes were being read.
        if self.key(pid).ok() != Some(key) {
            return None;
        }
        self.cache.insert(
            key,
            Cached {
                info: info.clone(),
                refreshed: now,
                last_used: now,
            },
        );
        Some(info)
    }
    pub fn read(&mut self, pid: u32, now: Instant) -> Option<ProcInfo> {
        let mut info = self.one(pid, now)?;
        let mut next = info.ppid;
        for _ in 0..5 {
            if next == 0 || next == info.key.pid {
                break;
            }
            let Some(a) = self.one(next, now) else {
                break;
            };
            next = a.ppid;
            info.ancestors.push(a);
        }
        Some(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_fixture_cmdline_cap() {
        let tree = super::fixture::Tree::new();
        tree.process(100, 1, 50, "node", "/repo", &["node"], "/app.service");
        fs::write(tree.0.join("100/cmdline"), vec![b'a'; 20000]).unwrap();
        let mut reader = ProcReader::new(ProcRoot::new(&tree.0));
        let info = reader.read(100, Instant::now()).unwrap();
        assert_eq!(info.cmdline_raw.len(), 1);
        assert_eq!(info.cmdline_raw[0].len(), 16384);
    }
    #[test]
    fn discovery_stat_cgroup_cmdline() {
        let stat = format!("44 (npm (exec) vite) S 12 {} 12345 0", vec!["0"; 17].join(" "));
        let parsed = parse_stat(&stat).unwrap();
        assert_eq!(parsed.comm, "npm (exec) vite");
        assert_eq!(parsed.ppid, 12);
        assert_eq!(parsed.start_ticks, 12345);
        assert!(parse_stat("1 (bad) S").is_err());
        assert_eq!(
            unit_of(
                "0::/user.slice/user@1000.service/lantern.slice/lantern-inst-foo\\x2dbar.scope/tab(10).scope\n"
            ),
            Some("lantern-inst-foo-bar.scope".into())
        );
        assert_eq!(
            unit_of("0::/user.slice/user@1000.service/app-test.service/tab(2).scope"),
            Some("app-test.service".into())
        );
        assert_eq!(unit_of("1:name=systemd:/foo.service"), None);
        assert!(is_lantern_inst("lantern-inst-foo\\x2dbar.service"));
        assert!(!is_lantern_inst("lantern-svc-db.service"));
    }
}

#[cfg(test)]
pub(super) mod fixture {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    pub struct Tree(pub PathBuf);
    impl Tree {
        pub fn new() -> Self {
            let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/discovery-fixtures");
            let path = base.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("stat"), "btime 1700000000\n").unwrap();
            Self(path)
        }
        #[allow(clippy::too_many_arguments)]
        pub fn process(
            &self,
            pid: u32,
            ppid: u32,
            ticks: u64,
            comm: &str,
            cwd: &str,
            args: &[&str],
            unit: &str,
        ) {
            let path = self.0.join(pid.to_string());
            fs::create_dir_all(path.join("fd")).unwrap();
            let stat = format!("{pid} ({comm}) S {ppid} {} {ticks} 0\n", vec!["0"; 17].join(" "));
            fs::write(path.join("stat"), stat).unwrap();
            fs::write(path.join("cmdline"), format!("{}\0", args.join("\0"))).unwrap();
            fs::write(path.join("cgroup"), format!("0::{unit}\n")).unwrap();
            let _ = fs::remove_file(path.join("cwd"));
            symlink(cwd, path.join("cwd")).unwrap();
            let _ = fs::remove_file(path.join("exe"));
            symlink("/usr/bin/node (deleted)", path.join("exe")).unwrap();
        }
        pub fn socket(&self, pid: u32, fd: u32, inode: u64) {
            symlink(
                format!("socket:[{inode}]"),
                self.0.join(pid.to_string()).join("fd").join(fd.to_string()),
            )
            .unwrap();
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
