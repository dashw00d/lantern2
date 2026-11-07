use clap::Parser;
use lantern_cli::{CliAction, UnassembledSystem, args::Cli};
use lantern_client::{Client, ClientConfig};
use lantern_core::{ids::InstanceId, model::InstanceStatus, settings::Settings};
use lantern_daemon::{
    app::{DaemonArgs, run_until},
    runtime::systemd::UnitNames,
};
use std::{
    collections::BTreeMap,
    fs,
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
    process::Command,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Guard {
    unit: String,
    dir: PathBuf,
}
impl Drop for Guard {
    fn drop(&mut self) {
        for action in ["stop", "reset-failed"] {
            let _ = Command::new("systemctl")
                .args(["--user", action, "--", &self.unit])
                .output();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}
async fn cli(host: &str, arguments: &[&str]) -> String {
    let mut args = vec!["lantern", "--host", host];
    args.extend_from_slice(arguments);
    let parsed = Cli::try_parse_from(args).unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = lantern_cli::dispatch(parsed, &UnassembledSystem, &mut out, &mut err, false).await;
    assert!(
        matches!(result, CliAction::Exit(0)),
        "CLI failed: {:?}; stdout={}; stderr={}",
        result,
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    String::from_utf8(out).unwrap()
}
async fn wait_for_api(client: &Client) {
    for _ in 0..100 {
        if client.ping().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon API did not start");
}
fn command_output(unit: &str) -> String {
    let out = Command::new("systemctl")
        .args(["--user", "show", "--property=LoadState", "--value", "--", unit])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real system: isolated transient user unit, loopback TCP and journal"]
async fn app_real_system_cli_start_restart_logs_and_stop() {
    let nonce = std::process::id();
    let id_text = format!("e2e{nonce}");
    let id = InstanceId::parse(&id_text).unwrap();
    let prefix = format!("lantern-test-assembly-{nonce}-inst-");
    let names = UnitNames {
        instance_prefix: prefix.clone(),
        operation_prefix: format!("lantern-test-assembly-{nonce}-op-"),
        slice: "app-lantern-test.slice".into(),
    };
    let dir = std::env::temp_dir().join(format!("lantern-assembly-real-{nonce}"));
    let guard = Guard {
        unit: names.instance(&id),
        dir: dir.clone(),
    };
    let root = dir.join("project");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(dir.join("cache")).unwrap();
    fs::write(dir.join("cache/shell-env.sh"), "export PATH='/usr/bin:/bin'\n").unwrap();
    fs::write(
        root.join("lantern.yaml"),
        format!(
            "id: {id_text}\ntype: proxy\nrun:\n  cmd: 'python3 -m http.server ${{PORT}} --bind 127.0.0.1'\n"
        ),
    )
    .unwrap();
    let port = (39600..39650)
        .find(|port| TcpListener::bind((Ipv4Addr::LOCALHOST, *port)).is_ok())
        .expect("test API port");
    let mut env = BTreeMap::new();
    for (key, path) in [
        ("HOME", dir.clone()),
        ("LANTERN_CONFIG_DIR", dir.join("config")),
        ("LANTERN_STATE_DIR", dir.join("state")),
        ("LANTERN_DATA_DIR", dir.join("data")),
        ("LANTERN_CACHE_DIR", dir.join("cache")),
        ("LANTERN_RUNTIME_DIR", dir.join("runtime")),
    ] {
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    let mut settings = Settings::default();
    settings.daemon.port = port;
    settings.domains.tld = "assembly-test".into();
    settings.ports.range_start = 39670;
    settings.ports.range_end = 39699;
    settings.workspaces.roots.clear();
    settings.runtime.startup_timeout_secs = 15;
    settings.runtime.stop_timeout_secs = 3;
    settings.health.interval_secs = 60;
    let args = DaemonArgs {
        dev: false,
        environment: Some(env),
        settings: Some(settings),
        unit_names: Some(names),
        restrict_journal_to_units: true,
    };
    let host = format!("http://127.0.0.1:{port}");
    let client = Client::new(ClientConfig {
        base_url: Some(host.clone()),
    })
    .unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(run_until(args.clone(), stop.clone()));
    wait_for_api(&client).await;
    cli(&host, &["add", root.to_str().unwrap()]).await;
    cli(&host, &["on", &id_text]).await;
    let view = client.instance(&id).await.unwrap();
    assert_eq!(view.runtime.status, InstanceStatus::Running);
    let service_port = view.runtime.port.unwrap();
    assert!((39670..=39699).contains(&service_port));
    let listing = cli(&host, &["--json", "ls"]).await;
    assert!(
        listing.contains(&id_text) && listing.contains(&service_port.to_string()),
        "{listing}"
    );
    reqwest::get(format!("http://127.0.0.1:{service_port}/"))
        .await
        .unwrap();
    let mut logs = String::new();
    for _ in 0..30 {
        logs = cli(&host, &["logs", &id_text]).await;
        if logs.contains("GET /") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(logs.contains("GET /"), "no HTTP access log: {logs}");
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(command_output(&guard.unit), "loaded");
    let stop2 = CancellationToken::new();
    let task2 = tokio::spawn(run_until(args, stop2.clone()));
    wait_for_api(&client).await;
    assert_eq!(
        client.instance(&id).await.unwrap().runtime.status,
        InstanceStatus::Running
    );
    cli(&host, &["off", &id_text]).await;
    assert_eq!(
        client.instance(&id).await.unwrap().runtime.status,
        InstanceStatus::Stopped
    );
    stop2.cancel();
    task2.await.unwrap().unwrap();
    assert_ne!(command_output(&guard.unit), "loaded", "test unit still loaded");
}
