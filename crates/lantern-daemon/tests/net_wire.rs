use lantern_core::{dirs::Dirs, settings::Settings};
use lantern_daemon::{
    app::network::{self, CaddyApplier, NetworkHealth},
    net::sync::NetRouter,
};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn net_wire_missing_caddy_preserves_plan_and_discovers_installed_binary() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let dirs = Dirs {
        config: dir.join("config"),
        state: dir.join("state"),
        data: dir.join("data"),
    };
    let paths = network::paths(
        dirs.clone(),
        dir.join("home"),
        dir.join("runtime"),
        dir.join("cache"),
    )
    .unwrap();
    let health = Arc::new(NetworkHealth::default());
    let settings = Settings::default();
    let applier = CaddyApplier::new(
        String::new(),
        String::new(),
        paths.clone(),
        "lantern-test-wire-caddy.service".into(),
        "app-lantern-test.slice".into(),
        "glow".into(),
        network::render(&paths, &settings, dir.join("php.sock")),
        health.clone(),
    );
    let cancel = CancellationToken::new();
    let router = NetRouter::new(
        Arc::new(applier),
        "glow".into(),
        false,
        dir.join("php.sock"),
        cancel.clone(),
    );
    router.replace_instances(vec![]);
    router.flush().await.unwrap();
    assert!(
        router
            .current_plan()
            .routes
            .iter()
            .any(|r| r.host == "lantern.glow")
    );
    assert!(
        health
            .checks()
            .iter()
            .any(|check| check.id == "caddy_missing" && check.fix.is_some())
    );
    let executable = dirs.data.join("bin/caddy");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        network::resolve_caddy("", &dirs.data, ""),
        Some(executable.clone())
    );
    let path_binary = dir.join("path/caddy");
    std::fs::create_dir_all(path_binary.parent().unwrap()).unwrap();
    std::fs::write(&path_binary, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&path_binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        network::resolve_caddy(path_binary.to_str().unwrap(), &dirs.data, ""),
        Some(path_binary.clone())
    );
    assert_eq!(
        network::resolve_caddy(
            "",
            &dir.join("empty"),
            path_binary.parent().unwrap().to_str().unwrap()
        ),
        Some(path_binary)
    );
    cancel.cancel();
}
