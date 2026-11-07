//! Synchronous scanner intended for a blocking worker.
use super::build::{Observation, build};
use super::hide::HideSettings;
use super::matcher::CheckoutIndex;
use super::owners::Owners;
use super::procinfo::{ProcReader, ProcRoot};
use super::source::{AutoSource, RawSocket, SocketSource, SocketSourceKind};
use lantern_core::model::Listener;
use std::fmt;
use std::io;
use std::time::{Instant, SystemTime};

/// A stable discovery result; hidden rows are kept separately for explicit requests.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Increments only when visible listener rows change.
    pub seq: u64,
    /// Scan completion time.
    pub taken_at: SystemTime,
    /// Visible listeners sorted by port, address and pid.
    pub listeners: Vec<Listener>,
    /// Hidden listeners, with their reasons.
    pub hidden: Vec<Listener>,
    /// Socket source actually used.
    pub source: SocketSourceKind,
    /// Time spent scanning in milliseconds.
    pub scan_ms: f32,
}
enum Source {
    Auto(AutoSource),
    Fixed(Box<dyn SocketSource>, SocketSourceKind),
}
/// Blocking scanner with shared process and inode caches.
pub struct Scanner {
    source: Source,
    reader: ProcReader,
    owners: Owners,
    index: CheckoutIndex,
    settings: HideSettings,
    seq: u64,
    listeners: Vec<Listener>,
}
impl fmt::Debug for Scanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanner")
            .field("root", &self.reader.root.path())
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}
impl Scanner {
    /// Use sock_diag with automatic procfs fallback.
    pub fn new(root: ProcRoot, index: CheckoutIndex, settings: HideSettings) -> Self {
        let source = Source::Auto(AutoSource::new(root.path()));
        Self::init(root, index, settings, source)
    }
    /// Use an explicit socket source, including fixture and proc-only sources.
    pub fn with_source(
        root: ProcRoot,
        index: CheckoutIndex,
        settings: HideSettings,
        source: Box<dyn SocketSource>,
        kind: SocketSourceKind,
    ) -> Self {
        Self::init(root, index, settings, Source::Fixed(source, kind))
    }
    fn init(root: ProcRoot, index: CheckoutIndex, settings: HideSettings, source: Source) -> Self {
        Self {
            source,
            reader: ProcReader::new(root),
            owners: Owners::default(),
            index,
            settings,
            seq: 0,
            listeners: Vec::new(),
        }
    }
    /// Replace registry checkout data for subsequent scans.
    pub fn set_index(&mut self, index: CheckoutIndex) {
        self.index = index;
    }
    /// Replace discovery visibility configuration for subsequent scans.
    pub fn set_settings(&mut self, settings: HideSettings) {
        self.settings = settings;
    }
    fn sockets(&mut self) -> io::Result<(SocketSourceKind, Vec<RawSocket>)> {
        match &mut self.source {
            Source::Auto(source) => source.scan(),
            Source::Fixed(source, kind) => Ok((*kind, source.listening_tcp()?)),
        }
    }
    fn observe(&mut self, sockets: Vec<RawSocket>, now: Instant) -> Vec<Observation> {
        let owners = self.owners.resolve(&sockets, &mut self.reader, now);
        sockets
            .into_iter()
            .map(|s| {
                let info = owners.get(&s.inode).and_then(|pid| self.reader.read(*pid, now));
                (s, info)
            })
            .collect()
    }
    /// Perform a fresh blocking scan and increment sequence only for visible changes.
    pub fn scan(&mut self) -> io::Result<Snapshot> {
        let start = Instant::now();
        let (source, sockets) = self.sockets()?;
        let observations = self.observe(sockets, start);
        let (listeners, hidden) = build(
            observations,
            &self.index,
            &self.settings,
            self.reader.root.uid(),
            std::process::id(),
        );
        if listeners != self.listeners {
            self.seq = self.seq.saturating_add(1);
            self.listeners.clone_from(&listeners);
        }
        Ok(Snapshot {
            seq: self.seq,
            taken_at: SystemTime::now(),
            listeners,
            hidden,
            source,
            scan_ms: start.elapsed().as_secs_f32() * 1000.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::matcher::CheckoutRef;
    use crate::discovery::procinfo::fixture::Tree;
    use lantern_core::ids::{InstanceId, ProjectId};
    use lantern_core::model::{CheckoutKind, MatchSource};
    #[derive(Debug)]
    struct Fixed(Vec<RawSocket>);
    impl SocketSource for Fixed {
        fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>> {
            Ok(self.0.clone())
        }
    }
    #[test]
    fn discovery_snapshot_stable_grouping_hidden_and_redaction() {
        let t = Tree::new();
        t.process(
            111,
            1,
            10,
            "node",
            "/dev/app/src",
            &["node", "vite", "--token=secret"],
            "/app.service",
        );
        t.socket(111, 3, 123);
        t.socket(111, 4, 124);
        let root = ProcRoot::new(&t.0);
        let uid = root.uid();
        let id = ProjectId::parse("app").unwrap();
        let index = CheckoutIndex::new(
            [CheckoutRef {
                instance: InstanceId::primary(&id),
                project: id,
                kind: CheckoutKind::Primary,
                branch: Some("main".into()),
                path: "/dev/app".into(),
            }],
            "/home/test",
        );
        let sockets = vec![
            RawSocket {
                addr: "0.0.0.0:39200".parse().unwrap(),
                inode: 124,
                uid,
            },
            RawSocket {
                addr: "127.0.0.1:39200".parse().unwrap(),
                inode: 123,
                uid,
            },
            RawSocket {
                addr: "127.0.0.1:2019".parse().unwrap(),
                inode: 333,
                uid: uid + 1,
            },
        ];
        let mut scanner = Scanner::with_source(
            root,
            index,
            HideSettings::default(),
            Box::new(Fixed(sockets)),
            SocketSourceKind::ProcNetTcp,
        );
        let a = scanner.scan().unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(a.listeners.len(), 1);
        assert_eq!(a.hidden.len(), 1);
        let row = &a.listeners[0];
        assert_eq!(row.addr, "127.0.0.1:39200".parse().unwrap());
        assert_eq!(row.other_addrs, vec!["0.0.0.0:39200".parse().unwrap()]);
        assert_eq!(row.matched_by, Some(MatchSource::Cwd));
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert_eq!(row.cmdline[2], "--token=***");
        let b = scanner.scan().unwrap();
        assert_eq!(b.seq, a.seq);
        assert_eq!(b.listeners, a.listeners);
    }
}
