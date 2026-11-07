//! Real kernel socket enumeration acceptance tests.
use lantern_core::ids::{InstanceId, ProjectId};
use lantern_core::model::{CheckoutKind, MatchSource};
use lantern_daemon::discovery::{
    Scanner,
    hide::HideSettings,
    matcher::{CheckoutIndex, CheckoutRef},
    procinfo::ProcRoot,
    source::{ProcNetTcp, SockDiag, SocketSource, SocketSourceKind},
};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Instant;

fn listener() -> TcpListener {
    (39200..=39299)
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("a discovery test port in 39200..39299 must be free")
}
fn index() -> CheckoutIndex {
    let path = std::env::current_dir().unwrap().canonicalize().unwrap();
    let id = ProjectId::parse("discovery-test").unwrap();
    let checkout = CheckoutRef {
        instance: InstanceId::primary(&id),
        project: id,
        kind: CheckoutKind::Primary,
        branch: Some("test".into()),
        path,
    };
    CheckoutIndex::new(
        [checkout],
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent-home")),
    )
}
fn acceptance(source: Box<dyn SocketSource>, kind: SocketSourceKind) {
    let socket = listener();
    let port = socket.local_addr().unwrap().port();
    let settings = HideSettings {
        hide_defaults: false,
        ..HideSettings::default()
    };
    let mut scanner = Scanner::with_source(ProcRoot::default(), index(), settings, source, kind);
    let snapshot = scanner.scan().expect("socket source and owner scan must succeed");
    assert_eq!(snapshot.source, kind);
    let row = snapshot
        .listeners
        .iter()
        .find(|l| l.port == port && l.pid == Some(std::process::id()))
        .expect("test socket is attributed to this process");
    assert_eq!(
        row.cwd.as_deref(),
        Some(std::env::current_dir().unwrap().canonicalize().unwrap().as_path())
    );
    assert_eq!(row.addr, socket.local_addr().unwrap());
    assert_eq!(row.matched_by, Some(MatchSource::Cwd));
    assert_eq!(row.branch.as_deref(), Some("test"));
    println!(
        "{kind:?}: port {port}, pid {}, scan {:.3} ms",
        std::process::id(),
        snapshot.scan_ms
    );
}
#[test]
#[ignore = "real system: kernel netlink and loopback socket"]
fn discovery_real_sock_diag() {
    acceptance(Box::new(SockDiag), SocketSourceKind::SockDiag);
}
#[test]
#[ignore = "real system: procfs and loopback socket"]
fn discovery_real_proc_fallback() {
    acceptance(Box::new(ProcNetTcp::default()), SocketSourceKind::ProcNetTcp);
}
#[test]
#[ignore = "real system: scan timing against current procfs"]
fn discovery_real_scan_timing() {
    let socket = listener();
    let mut scanner = Scanner::new(
        ProcRoot::default(),
        index(),
        HideSettings {
            hide_defaults: false,
            ..HideSettings::default()
        },
    );
    let start = Instant::now();
    let first = scanner.scan().unwrap();
    let mut timings = Vec::new();
    for _ in 0..5 {
        timings.push(scanner.scan().unwrap().scan_ms);
    }
    println!(
        "discovery timing: source={:?} first={:.3} ms steady_ms={timings:?} total={:.3} ms port={}",
        first.source,
        first.scan_ms,
        start.elapsed().as_secs_f64() * 1000.0,
        socket.local_addr().unwrap().port()
    );
}
