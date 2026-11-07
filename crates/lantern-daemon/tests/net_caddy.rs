use lantern_core::{dirs::Dirs, ids::InstanceId, settings::ProxySettings};
use lantern_daemon::{
    net::{
        caddy::{ApplyOutcome, Caddy, CaddyConfig, NetError},
        caddy_config::RenderCtx,
        routes::{Route, RouteOwner, RoutePlan, Target},
    },
    runtime::systemd::FakeSystemd,
};
use lantern_system::paths::LanternPaths;
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

#[tokio::test]
async fn net_caddy_fake_admin_persists_only_accepted_configs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
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
    let socket = paths.caddy_admin.clone().unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let reject = Arc::new(AtomicBool::new(false));
    let posts = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn({
        let reject = reject.clone();
        let posts = posts.clone();
        async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let reject = reject.clone();
                let posts = posts.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 4096];
                    let (header_end, length) = loop {
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let h = String::from_utf8_lossy(&request[..pos]);
                            let length = h
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|n| n.parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            break (pos + 4, length);
                        }
                    };
                    while request.len() < header_end + length {
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                    }
                    let is_post = request.starts_with(b"POST /load ");
                    if is_post {
                        posts.fetch_add(1, Ordering::SeqCst);
                    }
                    let (status, body) = if is_post && reject.load(Ordering::SeqCst) {
                        ("400 Bad Request", "address already in use")
                    } else {
                        ("200 OK", "{}")
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        }
    });
    let render = RenderCtx {
        gateway: "127.0.0.1:39701".parse().unwrap(),
        admin_socket: socket,
        storage: paths.caddy_data.clone(),
        root_cert: paths.root_cert.clone(),
        root_key: paths.root_key.clone(),
        php_socket: root.join("fpm.sock"),
        php_env: BTreeMap::new(),
        proxy: ProxySettings {
            http_port: 39702,
            https_port: 39703,
            ..Default::default()
        },
    };
    let sd = Arc::new(FakeSystemd::default());
    sd.set_auto_start(true);
    let caddy = Caddy::new(
        CaddyConfig {
            binary: "/opt/caddy".into(),
            unit: "lantern-test-net-caddy.service".into(),
            slice: "app-lantern-test.slice".into(),
            tld: "glow".into(),
            render,
        },
        sd.clone(),
        paths.clone(),
    )
    .unwrap();
    caddy.ensure_running().await.unwrap();
    assert!(
        sd.calls()
            .iter()
            .any(|c| c == "start lantern-test-net-caddy.service")
    );
    let empty = RoutePlan::default();
    assert_eq!(caddy.apply(&empty).await.unwrap(), ApplyOutcome::Applied);
    let saved = std::fs::read(&paths.caddy_config).unwrap();
    assert_eq!(
        std::fs::metadata(&paths.caddy_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(caddy.apply(&empty).await.unwrap(), ApplyOutcome::Unchanged);
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    reject.store(true, Ordering::SeqCst);
    let changed = RoutePlan {
        routes: vec![Route {
            host: "app.glow".into(),
            owner: RouteOwner::Instance(InstanceId::parse("app").unwrap()),
            target: Target::Down,
        }],
        skipped: vec![],
    };
    assert!(matches!(caddy.apply(&changed).await, Err(NetError::PortInUse(_))));
    assert_eq!(saved, std::fs::read(&paths.caddy_config).unwrap());
    let status = caddy.status().await;
    assert!(status.running && status.admin_ok && status.last_error.is_some());
    server.abort();
}
