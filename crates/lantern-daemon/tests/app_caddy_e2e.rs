use axum::{
    Router,
    extract::ws::{Message as AxumMessage, WebSocketUpgrade},
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{FutureExt, SinkExt, StreamExt};
use lantern_core::{ids::InstanceId, settings::Settings};
use lantern_daemon::{
    app::{DaemonArgs, run_until},
    runtime::systemd::UnitNames,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    net::{Ipv4Addr, TcpListener, UdpSocket},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio_tungstenite::{Connector, client_async_tls_with_config, tungstenite::Message};
use tokio_util::sync::CancellationToken;

// The daemon launches this same test executable as its managed backend. The helper never
// starts unless invoked by that unit; this also makes a broad --ignored run safe.
#[test]
#[ignore = "real system: caddy"]
fn app_caddy_backend() {
    if std::env::var("LANTERN_CADDY_TEST_BACKEND").as_deref() != Ok("1") {
        return;
    }
    let port: u16 = std::env::var("PORT").unwrap().parse().unwrap();
    assert!((39960..=39989).contains(&port));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let instance = std::env::var("LANTERN_INSTANCE_ID").unwrap();
            let app = Router::new()
                .route(
                    "/",
                    get(move || {
                        let instance = instance.clone();
                        async move { format!("caddy backend {instance}") }
                    }),
                )
                .route(
                    "/ws",
                    get(|upgrade: WebSocketUpgrade| async move {
                        upgrade.on_upgrade(|mut socket| async move {
                            while let Some(Ok(message)) = socket.recv().await {
                                if let AxumMessage::Text(text) = message
                                    && socket.send(AxumMessage::Text(text)).await.is_err()
                                {
                                    break;
                                }
                            }
                        })
                    }),
                );
            let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            axum::serve(listener, app).await.unwrap();
        });
}

struct UnitGuard {
    units: Vec<String>,
    prefix: String,
    shutdown: CancellationToken,
}
impl UnitGuard {
    fn matching_units(&self) -> Result<Vec<String>, String> {
        let pattern = format!("{}*.service", self.prefix);
        let output = Command::new("systemctl")
            .args([
                "--user",
                "list-units",
                "--all",
                "--plain",
                "--no-legend",
                "--",
                &pattern,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "could not enumerate test units: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|unit| unit.starts_with(&self.prefix) && unit.ends_with(".service"))
            .map(str::to_owned)
            .collect())
    }
    fn cleanup(&self) {
        self.shutdown.cancel();
        let mut units = self.units.clone();
        units.extend(self.matching_units().unwrap_or_default());
        units.sort();
        units.dedup();
        for unit in &units {
            for action in ["stop", "reset-failed"] {
                let _ = Command::new("systemctl")
                    .args(["--user", action, "--", unit])
                    .output();
            }
        }
    }
    fn assert_gone(&self) {
        assert!(
            self.matching_units().expect("enumerate test units").is_empty(),
            "test units remain loaded"
        );
        for unit in &self.units {
            let output = Command::new("systemctl")
                .args(["--user", "show", "--property=LoadState", "--value", "--", unit])
                .output()
                .unwrap();
            assert_ne!(
                String::from_utf8_lossy(&output.stdout).trim(),
                "loaded",
                "unit {unit} still loaded"
            );
        }
    }
}
impl Drop for UnitGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn free_tcp(start: u16, end: u16) -> u16 {
    (start..=end)
        .find(|p| TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok())
        .expect("free TCP test port")
}
fn free_dns(start: u16, end: u16) -> u16 {
    (start..=end)
        .find(|p| {
            UdpSocket::bind((Ipv4Addr::LOCALHOST, *p)).is_ok()
                && TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok()
        })
        .expect("free DNS test port")
}
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .env("HOME", root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
async fn api_json(response: reqwest::Response) -> Value {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "API {status}: {body}");
    serde_json::from_str(&body).unwrap()
}
async fn wait_ready(client: &reqwest::Client, base: &str) {
    for _ in 0..150 {
        if client
            .get(format!("{base}/api/system/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon API did not become ready");
}
async fn wait_https(client: &reqwest::Client, url: &str, instance: &str) {
    let mut last = String::new();
    for _ in 0..200 {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                assert_eq!(
                    response.text().await.unwrap(),
                    format!("caddy backend {instance}")
                );
                return;
            }
            Ok(response) => last = format!("HTTP {}", response.status()),
            Err(error) => last = format!("{error:?}"),
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("{url} did not become ready: {last}");
}
fn unit_pid(unit: &str) -> u32 {
    let out = Command::new("systemctl")
        .args(["--user", "show", "--property=MainPID", "--value", "--", unit])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}
fn load_count(unit: &str) -> usize {
    let output = Command::new("journalctl")
        .args(["--user", "--no-pager", "-o", "cat", "-u", unit])
        .output()
        .unwrap();
    assert!(output.status.success(), "failed to read the test Caddy journal");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("\"method\": \"POST\"") && line.contains("\"uri\": \"/load\""))
        .count()
}

async fn admin_config(client: &reqwest::Client) -> Value {
    api_json(client.get("http://localhost/config/").send().await.unwrap()).await
}
fn contains_host(config: &Value, host: &str) -> bool {
    config["apps"]["http"]["servers"]["lantern-https"]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| {
            r["match"].as_array().is_some_and(|m| {
                m.iter()
                    .any(|m| m["host"].as_array().is_some_and(|h| h.iter().any(|v| v == host)))
            })
        })
}
async fn wait_host(admin: &reqwest::Client, host: &str, present: bool) {
    for _ in 0..150 {
        if let Ok(response) = admin.get("http://localhost/config/").send().await
            && let Ok(config) = response.json::<Value>().await
            && contains_host(&config, host) == present
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("host {host} did not reach present={present}");
}
async fn check_certificate(root: &Path, host: &str, port: u16) {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("openssl")
            .args([
                "s_client",
                "-connect",
                &format!("127.0.0.1:{port}"),
                "-servername",
                host,
                "-verify_hostname",
                host,
                "-verify_return_error",
                "-no-CApath",
                "-no-CAstore",
                "-CAfile",
            ])
            .arg(root)
            .arg("-showcerts")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "TLS chain {host}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let chain = String::from_utf8(output.stdout).unwrap();
    assert!(
        chain.matches("-----BEGIN CERTIFICATE-----").count() >= 2,
        "server must send leaf and intermediate: {chain}"
    );
    let leaf = chain
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .unwrap()
        .split("-----END CERTIFICATE-----")
        .next()
        .unwrap();
    let mut command = tokio::process::Command::new("openssl")
        .args(["x509", "-noout", "-ext", "subjectAltName"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    use tokio::io::AsyncWriteExt;
    command
        .stdin
        .take()
        .unwrap()
        .write_all(format!("-----BEGIN CERTIFICATE-----{leaf}-----END CERTIFICATE-----\n").as_bytes())
        .await
        .unwrap();
    let output = command.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let san = String::from_utf8(output.stdout).unwrap();
    assert!(san.contains(&format!("DNS:{host}")), "SAN {san}");
    println!("verified chain and SAN DNS:{host} against Lantern CA only");
}
fn tls_config(root: &[u8]) -> Arc<rustls::ClientConfig> {
    let pem = std::str::from_utf8(root).unwrap();
    let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
    let der = STANDARD.decode(body).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(der)).unwrap();
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

struct Scenario<'a> {
    client: &'a reqwest::Client,
    base: &'a str,
    root: &'a Path,
    ca: &'a Path,
    admin_socket: &'a Path,
    unit: &'a str,
    http_port: u16,
    https_port: u16,
}
async fn scenario(s: Scenario<'_>) {
    wait_ready(s.client, s.base).await;
    let register = s
        .client
        .post(format!("{}/api/projects", s.base))
        .json(&json!({"path":s.root}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        register.status(),
        201,
        "register: {}",
        register.text().await.unwrap()
    );
    let start = s
        .client
        .post(format!(
            "{}/api/instances/myapp/start?wait=true&timeout=40",
            s.base
        ))
        .send()
        .await
        .unwrap();
    let started = api_json(start).await;
    assert_eq!(started["operation"]["status"], "succeeded", "{started}");
    for _ in 0..150 {
        if s.ca.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ca = fs::read(s.ca).expect("Lantern CA generated");
    let https = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .tls_certs_only([reqwest::Certificate::from_pem(&ca).unwrap()])
        .resolve("myapp.glow2", (Ipv4Addr::LOCALHOST, s.https_port).into())
        .resolve(
            "feature-x.myapp.glow2",
            (Ipv4Addr::LOCALHOST, s.https_port).into(),
        )
        .build()
        .unwrap();
    let primary = format!("https://myapp.glow2:{}/", s.https_port);
    let branch = format!("https://feature-x.myapp.glow2:{}/", s.https_port);
    wait_https(&https, &primary, "myapp").await;
    let untrusted = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .tls_certs_only([])
        .resolve("myapp.glow2", (Ipv4Addr::LOCALHOST, s.https_port).into())
        .build()
        .unwrap();
    assert!(
        untrusted.get(&primary).send().await.is_err(),
        "an empty trust store must reject the Lantern chain"
    );
    println!("empty trust store rejected the Lantern chain");
    check_certificate(s.ca, "myapp.glow2", s.https_port).await;
    let admin = reqwest::Client::builder()
        .no_proxy()
        .unix_socket(s.admin_socket.to_path_buf())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    assert!(contains_host(&admin_config(&admin).await, "myapp.glow2"));
    let before_loads = load_count(s.unit);
    let before_pid = unit_pid(s.unit);
    assert!(before_pid > 0);
    let tcp = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, s.https_port))
        .await
        .unwrap();
    let (mut ws, _) = client_async_tls_with_config(
        format!("wss://myapp.glow2:{}/ws", s.https_port),
        tcp,
        None,
        Some(Connector::Rustls(tls_config(&ca))),
    )
    .await
    .unwrap();
    ws.send(Message::Text("before reload".into())).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().into_text().unwrap(),
        "before reload"
    );
    let response = s
        .client
        .post(format!(
            "{}/api/projects/myapp/instances?wait=true&timeout=50",
            s.base
        ))
        .json(&json!({"branch":"feature-x","fetch":false,"start":true}))
        .send()
        .await
        .unwrap();
    let branch_op = api_json(response).await;
    assert_eq!(branch_op["operation"]["status"], "succeeded", "{branch_op}");
    wait_host(&admin, "feature-x.myapp.glow2", true).await;
    wait_https(&https, &branch, "myapp@feature-x").await;
    check_certificate(s.ca, "feature-x.myapp.glow2", s.https_port).await;
    assert_eq!(
        unit_pid(s.unit),
        before_pid,
        "route reload must not restart Caddy"
    );
    let mut load_observed = false;
    for _ in 0..20 {
        if load_count(s.unit) > before_loads {
            load_observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        load_observed,
        "the branch route must be delivered through POST /load"
    );
    ws.send(Message::Text("after reload".into())).await.unwrap();
    let echo = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echo.into_text().unwrap(), "after reload");
    println!("WebSocket survived live Caddy POST /load with branch route added");
    let redirect_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve("myapp.glow2", (Ipv4Addr::LOCALHOST, s.http_port).into())
        .build()
        .unwrap();
    let response = redirect_client
        .get(format!("http://myapp.glow2:{}/hello?q=1", s.http_port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 308);
    assert_eq!(
        response.headers()["location"],
        format!("https://myapp.glow2:{}/hello?q=1", s.https_port)
    );
    println!("HTTP redirect uses HTTPS test port");
    let kill = Command::new("systemctl")
        .args([
            "--user",
            "kill",
            "--kill-who=main",
            "--signal=SIGKILL",
            "--",
            s.unit,
        ])
        .output()
        .unwrap();
    assert!(
        kill.status.success(),
        "kill test Caddy: {}",
        String::from_utf8_lossy(&kill.stderr)
    );
    let mut recovered = false;
    for _ in 0..300 {
        let pid = unit_pid(s.unit);
        if pid != 0
            && pid != before_pid
            && https
                .get(&primary)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
        {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(recovered, "Caddy did not restart and recover its route");
    wait_https(&https, &branch, "myapp@feature-x").await;
    println!("Caddy recovered after SIGKILL with both routes");
    // A clean stop disables systemd's Restart=on-failure. Only the daemon's periodic
    // reconciliation can recreate this collected transient unit.
    let stopped_pid = unit_pid(s.unit);
    let stop_caddy = Command::new("systemctl")
        .args(["--user", "stop", "--", s.unit])
        .output()
        .unwrap();
    assert!(
        stop_caddy.status.success(),
        "stop test Caddy: {}",
        String::from_utf8_lossy(&stop_caddy.stderr)
    );
    assert_eq!(unit_pid(s.unit), 0);
    let mut reconciled = false;
    for _ in 0..350 {
        let pid = unit_pid(s.unit);
        if pid != 0
            && pid != stopped_pid
            && https
                .get(&primary)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
        {
            reconciled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reconciled, "daemon reconciliation did not recreate stopped Caddy");
    wait_https(&https, &branch, "myapp@feature-x").await;
    println!("daemon reconciliation recreated a cleanly stopped Caddy and restored both routes");
    let stop = s
        .client
        .post(format!(
            "{}/api/instances/myapp@feature-x/stop?wait=true&timeout=40",
            s.base
        ))
        .send()
        .await
        .unwrap();
    let stopped = api_json(stop).await;
    assert_eq!(stopped["operation"]["status"], "succeeded", "{stopped}");
    let mut down = false;
    for _ in 0..100 {
        if https.get(&branch).send().await.is_ok_and(|r| r.status() == 503) {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(down, "stopped instance must no longer proxy to the backend");
    let remove = s
        .client
        .delete(format!(
            "{}/api/instances/myapp@feature-x?force=true&wait=true&timeout=40",
            s.base
        ))
        .send()
        .await
        .unwrap();
    let removed = api_json(remove).await;
    assert_eq!(removed["operation"]["status"], "succeeded", "{removed}");
    wait_host(&admin, "feature-x.myapp.glow2", false).await;
    assert!(contains_host(&admin_config(&admin).await, "myapp.glow2"));
    println!("removing branch removed its route");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real system: caddy"]
async fn app_caddy_real_system_https_reload_recovery_and_removal() {
    let caddy = PathBuf::from(
        std::env::var("LANTERN_TEST_CADDY").expect("set LANTERN_TEST_CADDY to the verified upstream Caddy"),
    );
    assert!(caddy.is_absolute() && caddy.is_file());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(temp.path().join("cache")).unwrap();
    fs::write(
        temp.path().join("cache/shell-env.sh"),
        "export PATH='/usr/bin:/bin'\n",
    )
    .unwrap();
    let exe = std::env::current_exe().unwrap();
    let cmd = format!(
        "LANTERN_CADDY_TEST_BACKEND=1 '{}' --ignored --exact app_caddy_backend --nocapture",
        exe.display()
    );
    fs::write(
        root.join("lantern.yaml"),
        format!("id: myapp\ntype: proxy\nrun: {{cmd: \"{cmd}\"}}\n"),
    )
    .unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "lantern.yaml"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
    git(&root, &["branch", "feature-x"]);
    let mut random = [0; 8];
    getrandom::fill(&mut random).unwrap();
    let nonce = format!("{}-{:x}", std::process::id(), u64::from_le_bytes(random));
    let names = UnitNames {
        instance_prefix: format!("lantern-test-caddy-{nonce}-inst-"),
        operation_prefix: format!("lantern-test-caddy-{nonce}-op-"),
        slice: "app-lantern-test.slice".into(),
    };
    let caddy_unit = format!("{}caddy.service", names.operation_prefix);
    let shutdown = CancellationToken::new();
    let guard = UnitGuard {
        units: vec![
            names.instance(&InstanceId::parse("myapp").unwrap()),
            names.instance(&InstanceId::parse("myapp@feature-x").unwrap()),
            caddy_unit.clone(),
        ],
        prefix: format!("lantern-test-caddy-{nonce}-"),
        shutdown: shutdown.clone(),
    };
    let mut env = BTreeMap::new();
    for (key, path) in [
        ("HOME", temp.path().to_path_buf()),
        ("LANTERN_CONFIG_DIR", temp.path().join("config")),
        ("LANTERN_STATE_DIR", temp.path().join("state")),
        ("LANTERN_DATA_DIR", temp.path().join("data")),
        ("LANTERN_CACHE_DIR", temp.path().join("cache")),
        ("LANTERN_RUNTIME_DIR", temp.path().join("run")),
    ] {
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    env.insert("PATH".into(), "/usr/bin:/bin".into());
    let api_port = free_tcp(39900, 39909);
    let dns_port = free_dns(39910, 39919);
    let http_port = free_tcp(39920, 39929);
    let https_port = free_tcp(39930, 39939);
    let mut settings = Settings::default();
    settings.daemon.port = api_port;
    settings.dns.listen = format!("127.0.0.1:{dns_port}");
    settings.domains.tld = "glow2".into();
    settings.proxy.http_port = http_port;
    settings.proxy.https_port = https_port;
    let dev_binary = temp.path().join("data/bin/caddy");
    fs::create_dir_all(dev_binary.parent().unwrap()).unwrap();
    fs::copy(&caddy, &dev_binary).unwrap();
    settings.ports.range_start = 39960;
    settings.ports.range_end = 39989;
    settings.workspaces.roots.clear();
    settings.runtime.startup_timeout_secs = 20;
    settings.runtime.stop_timeout_secs = 3;
    settings.health.interval_secs = 60;
    let args = DaemonArgs {
        dev: true,
        environment: Some(env),
        settings: Some(settings),
        unit_names: Some(names),
        restrict_journal_to_units: true,
    };
    let mut task = tokio::spawn(run_until(args, shutdown.clone()));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{api_port}");
    let ca = temp.path().join("data/caddy/pki/authorities/local/root.crt");
    let admin_socket = temp.path().join("run/caddy-admin.sock");
    let outcome = AssertUnwindSafe(scenario(Scenario {
        client: &client,
        base: &base,
        root: &root,
        ca: &ca,
        admin_socket: &admin_socket,
        unit: &caddy_unit,
        http_port,
        https_port,
    }))
    .catch_unwind()
    .await;
    shutdown.cancel();
    let daemon = tokio::time::timeout(Duration::from_secs(20), &mut task).await;
    if daemon.is_err() {
        task.abort();
        let _ = task.await;
    }
    if outcome.is_err() {
        eprintln!("daemon result: {daemon:?}");
        for unit in &guard.units {
            let log = Command::new("journalctl")
                .args(["--user", "--no-pager", "-n", "40", "-u", unit])
                .output()
                .unwrap();
            eprintln!("journal {unit}: {}", String::from_utf8_lossy(&log.stdout));
        }
    }
    guard.cleanup();
    guard.assert_gone();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    daemon.expect("daemon shutdown timed out").unwrap().unwrap();
    println!("all test units stopped and reset-failed");
}
