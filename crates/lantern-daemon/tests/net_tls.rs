use lantern_core::dirs::Dirs;
use lantern_daemon::net::tls::{RootStatus, StoreState, Tls};
use lantern_system::{
    ca::CaState,
    command::{CommandOutput, CommandRunner, CommandSpec},
    paths::LanternPaths,
};
use std::sync::Arc;
#[derive(Debug)]
struct NoCommands;
impl CommandRunner for NoCommands {
    fn run(&self, _: &CommandSpec) -> std::io::Result<CommandOutput> {
        panic!("unexpected command")
    }
}
#[test]
fn net_tls_creates_and_inspects_constrained_ca_without_trust_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dirs = Dirs {
        config: root.join("config"),
        state: root.join("state"),
        data: root.join("data"),
    };
    let paths = LanternPaths::new(
        dirs,
        root.join("home"),
        Some(root.join("run")),
        root.join("cache"),
        Some(root.join("system")),
    )
    .unwrap();
    let tls = Tls::new(paths.clone(), "glow".into(), Arc::new(NoCommands), None);
    assert_eq!(tls.status().ca, RootStatus::Missing);
    assert!(matches!(tls.ensure_ca().unwrap(), CaState::Created(_)));
    let status = tls.status();
    assert!(
        matches!(status.ca,RootStatus::Ok(ref info) if info.permitted_subtree==".glow" && info.constraint_critical)
    );
    assert!(!status.system_trusted);
    assert_eq!(status.stores[0].state, StoreState::NoCertutil);
    assert_eq!(tls.root_cert_path(), paths.root_cert);
    assert!(matches!(tls.ensure_ca().unwrap(), CaState::Existing(_)));
    let wrong = Tls::new(paths, "test".into(), Arc::new(NoCommands), None);
    assert!(matches!(wrong.status().ca, RootStatus::Invalid(_)));
    assert!(wrong.ensure_ca().is_err());
}
