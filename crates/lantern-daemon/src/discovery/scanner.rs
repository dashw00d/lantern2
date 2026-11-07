//! Synchronous scanner intended for a blocking worker.
use super::build::{Observation, build};
use super::docker::DockerCache;
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
    docker: DockerCache,
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
            docker: DockerCache::default(),
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
        let docker = self.docker.enrich(&sockets, &self.reader.root);
        sockets
            .into_iter()
            .map(|s| {
                let info = owners
                    .get(&s.inode)
                    .and_then(|key| self.reader.read(key.pid, now).filter(|proc| proc.key == *key));
                let docker = docker.get(&s.inode).cloned();
                (s, info, docker)
            })
            .collect()
    }
    /// Perform a fresh blocking readiness check without trusting a cached snapshot.
    pub fn port_binding(&mut self, port: u16, expected_unit: Option<&str>) -> io::Result<super::PortBinding> {
        let (_, mut sockets) = self.sockets()?;
        sockets.retain(|s| s.addr.port() == port);
        if sockets.is_empty() {
            return Ok(super::PortBinding::Free);
        }
        let observations = self.observe(sockets, Instant::now());
        let (visible, hidden) = build(
            observations,
            &self.index,
            &self.settings,
            self.reader.root.uid(),
            std::process::id(),
        );
        let rows: Vec<_> = visible.into_iter().chain(hidden).collect();
        if let Some(expected) = expected_unit.map(super::procinfo::normalize_unit) {
            let matches: Vec<_> = rows
                .iter()
                .filter(|l| {
                    l.pid.is_some()
                        && l.unit
                            .as_deref()
                            .is_some_and(|unit| super::procinfo::normalize_unit(unit) == expected)
                })
                .collect();
            if let Some(first) = matches.first() {
                let mut addrs: Vec<_> = matches
                    .iter()
                    .flat_map(|l| std::iter::once(l.addr).chain(l.other_addrs.iter().copied()))
                    .collect();
                addrs.sort_unstable();
                addrs.dedup();
                return Ok(super::PortBinding::BoundByUnit {
                    pid: first.pid.expect("filtered pid"),
                    addrs,
                });
            }
        }
        let first = rows.first().expect("nonempty sockets yield listeners");
        Ok(super::PortBinding::HeldByOther(first.into()))
    }
    /// Perform a fresh blocking scan and increment sequence only for visible changes.
    pub fn scan(&mut self) -> io::Result<Snapshot> {
        let start = Instant::now();
        let (source, sockets) = self.sockets()?;
        let source_ms = start.elapsed().as_secs_f32() * 1000.0;
        let observe_start = Instant::now();
        let observations = self.observe(sockets, start);
        let observe_ms = observe_start.elapsed().as_secs_f32() * 1000.0;
        let build_start = Instant::now();
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
        let scan_ms = start.elapsed().as_secs_f32() * 1000.0;
        if scan_ms > 50.0 {
            eprintln!(
                "lantern discovery: slow scan {scan_ms:.1} ms (source {source_ms:.1} ms, proc {observe_ms:.1} ms, build {:.1} ms)",
                build_start.elapsed().as_secs_f32() * 1000.0
            );
        }
        Ok(Snapshot {
            seq: self.seq,
            taken_at: SystemTime::now(),
            listeners,
            hidden,
            source,
            scan_ms,
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
    fn discovery_foreign_docker_is_labelled_without_pid() {
        use lantern_core::model::ListenerClass;
        let t = Tree::new();
        t.process(
            400,
            1,
            20,
            "docker-proxy",
            "/",
            &[
                "docker-proxy",
                "-proto",
                "tcp",
                "-host-ip",
                "127.0.0.1",
                "-host-port",
                "39203",
                "-container-port",
                "80",
            ],
            "/system.slice/docker.service",
        );
        std::fs::write(t.0.join("400/comm"), "docker-proxy\n").unwrap();
        let root = ProcRoot::new(&t.0);
        let uid = root.uid();
        let sockets = vec![RawSocket {
            addr: "127.0.0.1:39203".parse().unwrap(),
            uid: uid + 1,
            inode: 987,
        }];
        let settings = HideSettings {
            hide_defaults: false,
            ..HideSettings::default()
        };
        let mut scanner = Scanner::with_source(
            root,
            CheckoutIndex::new([], "/home/test"),
            settings,
            Box::new(Fixed(sockets)),
            SocketSourceKind::ProcNetTcp,
        );
        let snapshot = scanner.scan().unwrap();
        let row = &snapshot.listeners[0];
        assert_eq!(row.class, ListenerClass::Docker);
        assert_eq!(row.pid, None);
        assert_eq!(row.label, "Docker");
        assert_eq!(row.docker.as_ref().unwrap().container_port, Some(80));
        let holder: super::super::PortHolder = row.into();
        assert_eq!(
            serde_json::from_str::<super::super::PortHolder>(&serde_json::to_string(&holder).unwrap())
                .unwrap(),
            holder
        );
    }
    #[test]
    fn discovery_user_hide_and_settings_change() {
        use crate::discovery::hide::{HideRule, PortSpec};
        let t = Tree::new();
        t.process(
            112,
            1,
            10,
            "node",
            "/dev/app/tmp",
            &["node", "vite", "--type=renderer", "--token=secret"],
            "/app.service",
        );
        t.socket(112, 3, 888);
        let root = ProcRoot::new(&t.0);
        let uid = root.uid();
        let sockets = vec![RawSocket {
            addr: "127.0.0.1:39200".parse().unwrap(),
            inode: 888,
            uid,
        }];
        let settings = HideSettings {
            hide_defaults: false,
            rules: vec![HideRule {
                port: Some(PortSpec::Range(39199, 39201)),
                cmdline: Some("*--token=secret*".into()),
                cwd: Some("/dev/app".into()),
                reason: Some("fixture".into()),
                ..HideRule::default()
            }],
            ..HideSettings::default()
        };
        let mut scanner = Scanner::with_source(
            root,
            CheckoutIndex::new([], "/home/test"),
            settings,
            Box::new(Fixed(sockets)),
            SocketSourceKind::ProcNetTcp,
        );
        let hidden = scanner.scan().unwrap();
        assert!(hidden.listeners.is_empty());
        assert_eq!(hidden.hidden[0].hidden.as_ref().unwrap().why, "fixture");
        assert_eq!(hidden.hidden[0].cmdline[3], "--token=***");
        scanner.set_settings(HideSettings {
            hide_defaults: false,
            ..HideSettings::default()
        });
        let visible = scanner.scan().unwrap();
        assert_eq!(visible.listeners.len(), 1);
        assert_eq!(visible.seq, hidden.seq + 1);
    }
    #[test]
    fn discovery_port_binding_and_conflicts() {
        use crate::discovery::{
            PortBinding,
            hide::{HideRule, PortSpec},
        };
        let t = Tree::new();
        t.process(
            111,
            1,
            10,
            "node",
            "/dev/app",
            &["node", "--token", "secret"],
            "/lantern-inst-app.service",
        );
        t.socket(111, 3, 123);
        let root = ProcRoot::new(&t.0);
        let uid = root.uid();
        let project = ProjectId::parse("app").unwrap();
        let checkout = CheckoutRef {
            instance: InstanceId::primary(&project),
            project,
            kind: CheckoutKind::Primary,
            branch: None,
            path: "/dev/app".into(),
        };
        let instance = checkout.instance.clone();
        let index = CheckoutIndex::new([checkout], "/home/test");
        let sockets = vec![
            RawSocket {
                addr: "127.0.0.1:39200".parse().unwrap(),
                inode: 123,
                uid,
            },
            RawSocket {
                addr: "127.0.0.1:39201".parse().unwrap(),
                inode: 222,
                uid: uid + 1,
            },
        ];
        let settings = HideSettings {
            rules: vec![
                HideRule {
                    port: Some(PortSpec::Single(39200)),
                    ..HideRule::default()
                },
                HideRule {
                    port: Some(PortSpec::Single(39201)),
                    ..HideRule::default()
                },
            ],
            ..HideSettings::default()
        };
        let mut scanner = Scanner::with_source(
            root,
            index,
            settings,
            Box::new(Fixed(sockets)),
            SocketSourceKind::ProcNetTcp,
        );
        assert_eq!(scanner.port_binding(39202, None).unwrap(), PortBinding::Free);
        assert_eq!(
            scanner
                .port_binding(39200, Some("lantern-inst-app.service"))
                .unwrap(),
            PortBinding::BoundByUnit {
                pid: 111,
                addrs: vec!["127.0.0.1:39200".parse().unwrap()]
            }
        );
        let PortBinding::HeldByOther(holder) = scanner.port_binding(39200, None).unwrap() else {
            panic!("expected holder");
        };
        assert_eq!(holder.cmdline[2], "***");
        let snapshot = scanner.scan().unwrap();
        assert_eq!(snapshot.listeners.len(), 1); // managed rows survive all hide rules
        assert_eq!(snapshot.hidden.len(), 1);
        assert!(snapshot.conflicts(&[(instance.clone(), 39200)]).is_empty());
        assert_eq!(snapshot.conflicts(&[(instance, 39201)])[0].holder.uid, uid + 1);
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
