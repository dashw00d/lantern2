use lantern_core::{dirs::Dirs, ids::InstanceId, settings::ProxySettings};
use lantern_daemon::{
    net::{
        caddy::{
            ApplyOutcome, Caddy, CaddyConfig, CaddyStartFuture, CaddyStarter, CaddyUnit, NetError,
            caddy_start_args,
        },
        caddy_config::RenderCtx,
        routes::{Route, RouteOwner, RoutePlan, Target},
    },
    runtime::systemd::{FakeSystemd, UnitStart, UserSystemd},
};
use lantern_system::paths::LanternPaths;
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

#[derive(Debug)]
struct FakeStarter(Arc<FakeSystemd>);
impl CaddyStarter for FakeStarter {
    fn start<'a>(&'a self, unit: &'a CaddyUnit) -> CaddyStartFuture<'a> {
        Box::pin(async move {
            let args = caddy_start_args(unit)?;
            assert!(args.iter().any(|arg| arg == "--property=Restart=on-failure"));
            assert!(args.iter().any(|arg| arg == "--property=RestartSec=1s"));
            let spec = UnitStart {
                unit: unit.unit.clone(),
                instance: InstanceId::parse("caddy").unwrap(),
                slice: unit.slice.clone(),
                cwd: unit.cwd.clone(),
                script: unit.config.clone(),
                post_stop: None,
                stop_timeout: std::time::Duration::from_secs(10),
            };
            self.0
                .start(&spec)
                .await
                .map_err(|e| NetError::Unavailable(e.to_string()))
        })
    }
}

#[test]
fn net_caddy_unit_arguments_escape_specifiers() {
    let unit = CaddyUnit {
        unit: "lantern-test-net-a.service".into(),
        slice: "app-lantern-test.slice".into(),
        binary: "/tmp/cad%dy".into(),
        config: "/tmp/config %x.json".into(),
        cwd: "/tmp".into(),
    };
    let args = caddy_start_args(&unit).unwrap();
    assert!(args.iter().any(|s| s == "/tmp/cad%%dy"));
    assert!(args.iter().any(|s| s == "/tmp/config %%x.json"));
    assert!(args.iter().any(|s| s == "--property=Restart=on-failure"));
}

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
    let current = Arc::new(Mutex::new("{}".to_owned()));
    let server = tokio::spawn({
        let reject = reject.clone();
        let posts = posts.clone();
        let current = current.clone();
        async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let reject = reject.clone();
                let posts = posts.clone();
                let current = current.clone();
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
                        ("400 Bad Request", "address already in use".to_owned())
                    } else if is_post {
                        *current.lock().unwrap() =
                            String::from_utf8(request[header_end..header_end + length].to_vec()).unwrap();
                        ("200 OK", "{}".to_owned())
                    } else {
                        ("200 OK", current.lock().unwrap().clone())
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
    .unwrap()
    .with_starter(Arc::new(FakeStarter(sd.clone())));
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
    *current.lock().unwrap() = "{}".into();
    assert_eq!(caddy.apply(&empty).await.unwrap(), ApplyOutcome::Applied);
    assert_eq!(posts.load(Ordering::SeqCst), 2);
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
