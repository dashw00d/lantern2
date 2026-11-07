use clap::{CommandFactory, Parser};
use lantern_cli::{CliAction, UnassembledSystem, args::Cli, dispatch};
use lantern_core::{
    api::{DiagnosticCounts, InstanceView, ManifestSummary, ProjectSummary},
    ids::{InstanceId, ProjectId},
    model::{
        Checkout, CheckoutKind, InstanceStatus, Project, ProjectKind, ProjectSource, RuntimeState, ServeMode,
    },
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

static NETWORK_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture() -> (ProjectSummary, InstanceView) {
    let project = ProjectId::parse("myapp").unwrap();
    let id = InstanceId::primary(&project);
    let timestamp = "2026-01-02T03:04:05Z".parse().unwrap();
    let view = InstanceView {
        id: id.clone(),
        project: project.clone(),
        checkout: Checkout {
            path: "/tmp/fake-myapp".into(),
            git_root: None,
            kind: CheckoutKind::Primary,
            branch: Some("main".into()),
            head: None,
            dirty: Some(false),
            locked: None,
            missing: false,
        },
        domain: "myapp.glow".into(),
        url: "https://myapp.glow".into(),
        pinned: true,
        adopt: false,
        visible: true,
        setup_stale: false,
        runtime: RuntimeState {
            status: InstanceStatus::Running,
            port: Some(29001),
            ..RuntimeState::default()
        },
        manifest: ManifestSummary {
            id: project.clone(),
            serve: ServeMode::Proxy,
            run_cmd: Some("serve".into()),
            open_path: None,
            description: None,
            diagnostics: DiagnosticCounts {
                errors: 0,
                warnings: 0,
                info: 0,
            },
            manifest_rev: "rev1".into(),
        },
    };
    let summary = ProjectSummary {
        project: Project {
            id: project,
            aliases: vec![],
            name: "My App".into(),
            root: "/tmp/fake-myapp".into(),
            repo: None,
            kind: ProjectKind::Project,
            enabled: true,
            tags: vec![],
            registered_at: timestamp,
            source: ProjectSource::Manifest,
        },
        primary: id,
        status: InstanceStatus::Running,
        url: view.url.clone(),
        port: Some(29001),
        instance_count: 1,
        docs_count: 0,
        endpoints_count: 0,
        routing: None,
    };
    (summary, view)
}

fn bind() -> TcpListener {
    (39600..=39649)
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("lane port available")
}
fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..end]);
            let len = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|n| n.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + len {
                break;
            }
        }
    }
    let request = String::from_utf8(data).unwrap();
    let path = request.lines().next().unwrap().to_string();
    (path, request)
}
fn accept_bounded(listener: &TcpListener) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("fake server accept: {error}"),
        }
    }
}
fn serve<F>(count: usize, handler: F) -> (String, thread::JoinHandle<()>)
where
    F: Fn(usize, &str, &str) -> (u16, String, &'static str) + Send + 'static,
{
    let listener = bind();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let thread = thread::spawn(move || {
        for n in 0..count {
            let mut stream = accept_bounded(&listener);
            let (path, req) = read_request(&mut stream);
            let (status, body, content_type) = handler(n, &path, &req);
            write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (addr, thread)
}
async fn run(args: &[&str], host: &str, tty: bool) -> (i32, String, String) {
    let mut argv = vec!["lantern", "--host", host];
    argv.extend_from_slice(args);
    let cli = Cli::try_parse_from(argv).unwrap();
    let mut out = vec![];
    let mut err = vec![];
    let action = dispatch(cli, &UnassembledSystem, &mut out, &mut err, tty).await;
    let code = match action {
        CliAction::Exit(n) => n,
        other => panic!("unexpected action: {other:?}"),
    };
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn command_tree_and_aliases_parse() {
    Cli::command().debug_assert();
    let examples: &[&[&str]] = &[
        &["lantern"],
        &["lantern", "list", "-a"],
        &["lantern", "st", "myapp"],
        &["lantern", "up", "myapp", "myapp@feature-x", "--no-wait"],
        &["lantern", "down", "--all", "-y"],
        &["lantern", "restart", "myapp", "--wait", "--timeout", "90"],
        &["lantern", "run", "myapp@feature/x", "--new", "--base", "main"],
        &[
            "lantern",
            "logs",
            "@feature/x",
            "-f",
            "-n",
            "50",
            "--stream",
            "stderr",
            "-t",
        ],
        &["lantern", "wt", "add", "myapp", "branch", "--pr", "3", "--update"],
        &[
            "lantern",
            "branch",
            "rm",
            "myapp@x",
            "--force",
            "--delete-branch",
            "-y",
        ],
        &["lantern", "wt", "ls"],
        &["lantern", "wt", "branches", "myapp"],
        &["lantern", "wt", "pin", "myapp@x"],
        &["lantern", "wt", "unpin", "myapp@x"],
        &["lantern", "wt", "relabel", "myapp@x", "x"],
        &["lantern", "wt", "gc", "--apply"],
        &["lantern", "adopt", "3000"],
        &["lantern", "unadopt", "myapp"],
        &["lantern", "scan", "/tmp/a"],
        &["lantern", "add", "--write-manifest"],
        &["lantern", "open", "--editor", "--print"],
        &["lantern", "setup", "myapp", "--force"],
        &["lantern", "manifest", "check", "/tmp/lantern.yaml"],
        &["lantern", "manifest", "show"],
        &["lantern", "schema", "profile"],
        &["lantern", "project", "rm", "myapp"],
        &["lantern", "project", "enable", "myapp"],
        &["lantern", "project", "disable", "myapp"],
        &["lantern", "project", "show", "myapp"],
        &["lantern", "services", "start", "redis"],
        &["lantern", "health"],
        &["lantern", "init", "-y"],
        &["lantern", "uninstall", "--purge-data"],
        &["lantern", "migrate-v1", "--dry-run"],
        &["lantern", "daemon"],
        &["lantern", "daemon", "logs", "-f"],
        &["lantern", "gui", "--tray"],
        &["lantern", "mcp", "url"],
        &["lantern", "mcp", "install", "claude-code", "--scope", "project"],
        &["lantern", "env", "show"],
        &["lantern", "env", "refresh"],
        &["lantern", "config", "set", "tld", "glow"],
        &["lantern", "events", "--logs", "myapp"],
        &["lantern", "completions", "bash"],
    ];
    for argv in examples {
        Cli::try_parse_from(*argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
    }
    for argv in [
        &["lantern", "on"][..],
        &["lantern", "off", "--all", "myapp"][..],
        &["lantern", "restart", "x", "--wait", "--no-wait"][..],
        &["lantern", "run", "x@y", "--base", "main"][..],
        &["lantern", "setup", "x", "--timeout", "601"][..],
    ] {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
    assert!(Cli::try_parse_from(["lantern", "logs", "--json"]).unwrap().json);
}

#[tokio::test]
async fn json_and_human_list_are_stable() {
    let _guard = NETWORK_TESTS.lock().await;
    let (p, v) = fixture();
    for json_mode in [false, true] {
        let p = p.clone();
        let v = v.clone();
        let (host, server) = serve(3, move |_, path, _| {
            let body = if path.contains("/api/projects?") {
                json!({"items":[p]})
            } else if path.contains("/api/instances") {
                json!({"items":[v]})
            } else if path.contains("/api/listeners") {
                json!({"items":[]})
            } else {
                panic!("{path}")
            };
            (200, body.to_string(), "application/json")
        });
        let args = if json_mode {
            vec!["ls", "--json"]
        } else {
            vec!["ls"]
        };
        let (code, out, err) = run(&args, &host, false).await;
        server.join().unwrap();
        assert_eq!(code, 0, "{err}");
        assert!(err.is_empty());
        if json_mode {
            assert_eq!(
                serde_json::from_str::<Value>(&out).unwrap(),
                json!({"instances":[fixture().1],"listeners":[]})
            );
        } else {
            assert_eq!(
                out,
                "PROJECT      INSTANCE                 BRANCH              STATUS          PORT   URL\nmyapp        myapp                    main                ● running       29001  https://myapp.glow\n"
            );
        }
    }
}

#[tokio::test]
async fn manifest_checks_are_offline_and_located() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lantern.yaml");
    std::fs::write(&file, "id: myapp\nrun:\n  cmd: [bad, type]\n").unwrap();
    let file = file.to_str().unwrap();
    let (code, out, err) = run(&["manifest", "check", file], "http://127.0.0.1:39699", false).await;
    assert_eq!(code, 8, "{out} {err}");
    assert!(out.contains("lantern.yaml:3:"), "{out}");
    assert!(err.is_empty());
    let (code, out, _) = run(
        &["manifest", "check", file, "--json"],
        "http://127.0.0.1:39699",
        false,
    )
    .await;
    assert_eq!(code, 8);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["usable"], false);
}

#[tokio::test]
async fn daemon_down_returns_three_and_hint_in_json() {
    let _guard = NETWORK_TESTS.lock().await;
    let listener = bind();
    let host = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (code, out, err) = run(&["ls", "--json"], &host, false).await;
    assert_eq!(code, 3);
    assert!(out.is_empty());
    let error: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(error["exit_code"], 3);
    assert_eq!(
        error["error"]["details"]["hint"],
        "systemctl --user start lantern"
    );
    let (code, _, err) = run(&["status", "myapp"], &host, false).await;
    assert_eq!(code, 3);
    assert!(err.contains("can't reach the Lantern daemon"));
    assert!(err.contains("systemctl --user start lantern"));
}

fn operation(status: &str) -> Value {
    json!({"operation":{"id":"op-1","kind":"start","instance":"myapp","status":status,"noop":false,"step":null,"steps":[],"created_at":"2026-01-02T03:04:05Z","finished_at":null,"error":null,"result":null},"instance":null,"created":null})
}
#[tokio::test]
async fn start_uses_daemon_wait_and_maps_operation_states() {
    let _guard = NETWORK_TESTS.lock().await;
    for (state, expected) in [("succeeded", 0), ("running", 6), ("failed", 5)] {
        let (_, view) = fixture();
        let response = operation(state);
        let (host, server) = serve(2, move |_, path, _| {
            if path.starts_with("GET /api/instances") {
                (200, json!({"items":[view]}).to_string(), "application/json")
            } else {
                assert!(path.starts_with("POST /api/instances/myapp/start?"), "{path}");
                assert!(path.contains("wait=true") && path.contains("timeout=120"));
                (200, response.to_string(), "application/json")
            }
        });
        let (code, out, err) = run(&["on", "myapp", "--json"], &host, false).await;
        server.join().unwrap();
        assert_eq!(code, expected, "{out} {err}");
        if expected == 0 {
            assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), operation(state));
        } else {
            assert!(out.is_empty());
            assert_eq!(
                serde_json::from_str::<Value>(&err).unwrap()["exit_code"],
                expected
            );
        }
    }
}

#[tokio::test]
async fn routes_branch_and_preflight_removal_to_server() {
    let _guard = NETWORK_TESTS.lock().await;
    let (_, view) = fixture();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_copy = seen.clone();
    let (host, server) = serve(2, move |_, path, req| {
        seen_copy
            .lock()
            .unwrap()
            .push((path.to_string(), req.to_string()));
        if path.starts_with("GET /api/instances") {
            (200, json!({"items":[view]}).to_string(), "application/json")
        } else {
            (202, operation("queued").to_string(), "application/json")
        }
    });
    let (code, _, err) = run(
        &[
            "run",
            "myapp@feature/login",
            "--new",
            "--base",
            "main",
            "--no-wait",
            "--json",
        ],
        &host,
        false,
    )
    .await;
    server.join().unwrap();
    assert_eq!(code, 0, "{err}");
    let requests = seen.lock().unwrap();
    assert!(requests[1].0.contains("wait=false"));
    assert!(requests[1].1.contains("\"new_branch\":\"feature/login\""));
}

#[tokio::test]
async fn exit_codes_and_noninteractive_confirmation() {
    let _guard = NETWORK_TESTS.lock().await;
    let (code, _, err) = run(&["off", "--all"], "http://127.0.0.1:39699", false).await;
    assert_eq!(code, 2);
    assert!(err.contains("requires -y"));
    let (code, _, err) = run(&["off", "--all", "--json"], "http://127.0.0.1:39699", true).await;
    assert_eq!(code, 2);
    assert_eq!(serde_json::from_str::<Value>(&err).unwrap()["exit_code"], 2);
    let (host, server) = serve(1, |_, _, _| {
        (200, json!({"items":[]}).to_string(), "application/json")
    });
    let (code, _, err) = run(&["on", "missing"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 4, "{err}");
    let (_, view) = fixture();
    let (host, server) = serve(2, move |_, path, _| {
        if path.starts_with("GET /api/instances") {
            (200, json!({"items":[view]}).to_string(), "application/json")
        } else {
            (
                409,
                json!({"error":{"code":"operation_in_progress","message":"already busy","details":null}})
                    .to_string(),
                "application/json",
            )
        }
    });
    let (code, _, err) = run(&["on", "myapp", "--json"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 7, "{err}");
    assert_eq!(
        serde_json::from_str::<Value>(&err).unwrap()["error"]["code"],
        "operation_in_progress"
    );
    let (host, server) = serve(1, |_, _, _| (200, "not JSON".into(), "application/json"));
    let (code, _, _) = run(&["on", "myapp"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 1);
}

#[tokio::test]
async fn operation_progress_comes_from_sse_while_waiting() {
    let _guard = NETWORK_TESTS.lock().await;
    let listener = bind();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let (_, view) = fixture();
    let paths = Arc::new(Mutex::new(Vec::new()));
    let server_paths = paths.clone();
    let server = thread::spawn(move || {
        let mut threads = vec![];
        for _ in 0..5 {
            let mut stream = accept_bounded(&listener);
            let view = view.clone();
            let paths = server_paths.clone();
            threads.push(thread::spawn(move || {
                let (path, _) = read_request(&mut stream); paths.lock().unwrap().push(path.clone());
                let (body, mime) = if path.starts_with("GET /api HTTP") {
                    (json!({"name":"Lantern","version":"0.2","api_version":1,"mcp":"/mcp","links":{}}).to_string(), "application/json")
                } else if path.starts_with("GET /api/instances") { (json!({"items":[view]}).to_string(), "application/json") }
                else if path.starts_with("POST /api/instances/myapp/start") { assert!(path.contains("wait=false")); (operation("queued").to_string(), "application/json") }
                else if path.starts_with("GET /api/events") {
                    assert!(path.contains("op=op-1") && path.contains("state=false"));
                    let event = json!({"type":"operation_progress","op":"op-1","instance":"myapp","step":"creating worktree","done":false,"error":null});
                    (format!("event: operation_progress\ndata: {event}\n\n"), "text/event-stream")
                } else if path.starts_with("GET /api/operations/op-1") {
                    thread::sleep(Duration::from_millis(200)); (operation("succeeded")["operation"].to_string(), "application/json")
                } else { panic!("unexpected request: {path}") };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
    });
    let (code, _, err) = run(&["on", "myapp", "--no-color"], &host, true).await;
    server.join().unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("creating worktree"), "{err}");
    assert_eq!(paths.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn follow_logs_use_sse_and_emit_safe_json_lines() {
    let _guard = NETWORK_TESTS.lock().await;
    let (_, view) = fixture();
    let (host, server) = serve(2, move |_, path, _| {
        if path.starts_with("GET /api/instances") {
            (200, json!({"items":[view]}).to_string(), "application/json")
        } else {
            assert!(
                path.starts_with("GET /api/events?")
                    && path.contains("logs=myapp")
                    && path.contains("tail=2"),
                "{path}"
            );
            let data = json!({"type":"log","instance":"myapp","stream":"stdout","line":"hello","ts":"2026-01-02T03:04:05Z"});
            (200, format!("event: log\ndata: {data}\n\n"), "text/event-stream")
        }
    });
    let cli = Cli::try_parse_from([
        "lantern", "--host", &host, "logs", "myapp", "-f", "-n", "2", "--json",
    ])
    .unwrap();
    let mut out = vec![];
    let mut err = vec![];
    let result = tokio::time::timeout(
        Duration::from_millis(250),
        dispatch(cli, &UnassembledSystem, &mut out, &mut err, false),
    )
    .await;
    assert!(result.is_err(), "follow should remain active");
    server.join().unwrap();
    let line: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(line["line"], "hello");
    assert_eq!(line["instance"], "myapp");
}

#[tokio::test]
async fn removal_preflight_blocks_dirty_managed_checkout() {
    let _guard = NETWORK_TESTS.lock().await;
    let (_, mut view) = fixture();
    view.id = InstanceId::derived(&view.project, "feature").unwrap();
    view.checkout.kind = CheckoutKind::Managed;
    view.checkout.branch = Some("feature".into());
    let (host, server) = serve(2, move |_, path, _| {
        if path.starts_with("GET /api/instances")
            && !path.contains("/logs?")
            && !path.contains("/removal-check")
        {
            (200, json!({"items":[view]}).to_string(), "application/json")
        } else {
            assert!(path.contains("/removal-check"));
            (200, json!({"blockers":[{"dirty":2}],"dirty":[],"unpushed":0,"orphaned":0,"has_submodules":false,"worktree_env":{},"branch":null}).to_string(), "application/json")
        }
    });
    let (code, _, err) = run(&["wt", "rm", "myapp@feature", "-y"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 7, "{err}");
    assert!(err.contains("blockers"));
}

#[tokio::test]
async fn log_backlog_removes_terminal_control_sequences() {
    let _guard = NETWORK_TESTS.lock().await;
    let (_, view) = fixture();
    let (host, server) = serve(2, move |_, path, _| {
        if path.starts_with("GET /api/instances")
            && !path.contains("/logs?")
            && !path.contains("/removal-check")
        {
            (200, json!({"items":[view]}).to_string(), "application/json")
        } else {
            (200, json!({"instance":"myapp","lines":[{"instance":"myapp","stream":"stdout","line":"hi\u{1b}[31mred\u{1b}[0m\u{1b}]0;evil\u{7}!","ts":"2026-01-02T03:04:05Z"}],"buffered":1,"truncated":false}).to_string(), "application/json")
        }
    });
    let (code, out, err) = run(&["logs", "myapp"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "03:04:05 out hired!\n");
}

#[tokio::test]
async fn branch_name_resolution_and_ambiguous_prefixes() {
    let _guard = NETWORK_TESTS.lock().await;
    let (_, primary) = fixture();
    let mut branch = primary.clone();
    branch.id = InstanceId::derived(&branch.project, "feature-login").unwrap();
    branch.checkout.branch = Some("feature/login".into());
    let views = vec![primary.clone(), branch.clone()];
    let (host, server) = serve(2, move |_, path, _| {
        if path.starts_with("GET /api/instances") {
            (200, json!({"items":views}).to_string(), "application/json")
        } else {
            assert!(
                path.starts_with("POST /api/instances/myapp%40feature-login/start"),
                "{path}"
            );
            (202, operation("queued").to_string(), "application/json")
        }
    });
    let (code, _, err) = run(&["on", "myapp@feature/login", "--no-wait"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 0, "{err}");
    let mut second = branch.clone();
    second.id = InstanceId::derived(&second.project, "feature-other").unwrap();
    second.checkout.branch = Some("feature/other".into());
    let (host, server) = serve(1, move |_, _, _| {
        (
            200,
            json!({"items":[primary,branch,second]}).to_string(),
            "application/json",
        )
    });
    let (code, _, err) = run(&["on", "myapp@feature"], &host, false).await;
    server.join().unwrap();
    assert_eq!(code, 4);
    assert!(err.contains("ambiguous") && err.contains("feature-other"));
}

#[tokio::test]
async fn scan_project_summary_has_stable_json_and_table() {
    let _guard = NETWORK_TESTS.lock().await;
    for as_json in [false, true] {
        let (project, _) = fixture();
        let project_copy = project.clone();
        let (host, server) = serve(1, move |_, path, _| {
            assert!(path.starts_with("POST /api/projects/scan"));
            (
                200,
                json!({"registered":[project],"already_known":2,"skipped":[]}).to_string(),
                "application/json",
            )
        });
        let args = if as_json {
            vec!["scan", "--json"]
        } else {
            vec!["scan"]
        };
        let (code, out, err) = run(&args, &host, false).await;
        server.join().unwrap();
        assert_eq!(code, 0, "{err}");
        if as_json {
            assert_eq!(
                serde_json::from_str::<Value>(&out).unwrap(),
                json!({"registered":[project_copy],"already_known":2,"skipped":[]})
            );
        } else {
            assert_eq!(
                out,
                "1 registered; 2 already known; 0 skipped\nPROJECT      STATUS          PORT   URL\nmyapp        ● running       29001  https://myapp.glow\n"
            );
        }
    }
}

#[tokio::test]
async fn system_boundary_is_offline_and_doctor_returns_problem_code() {
    #[derive(Debug)]
    struct FakeSystem;
    impl lantern_cli::SystemActions for FakeSystem {
        fn execute(&self, command: &lantern_cli::SystemCommand) -> Result<Value, String> {
            assert!(matches!(
                command,
                lantern_cli::SystemCommand::Doctor { fix: false }
            ));
            Ok(json!([{"id":"dns","status":"fail"}]))
        }
    }
    let cli = Cli::try_parse_from(["lantern", "doctor", "--json"]).unwrap();
    let mut out = vec![];
    let mut err = vec![];
    assert!(matches!(
        dispatch(cli, &FakeSystem, &mut out, &mut err, false).await,
        CliAction::Exit(8)
    ));
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap()[0]["status"],
        "fail"
    );
    assert!(err.is_empty());
}

#[tokio::test]
async fn process_actions_and_offline_commands_need_no_daemon() {
    let cli = Cli::try_parse_from(["lantern", "daemon"]).unwrap();
    let mut out = vec![];
    let mut err = vec![];
    assert!(matches!(
        dispatch(cli, &UnassembledSystem, &mut out, &mut err, false).await,
        CliAction::RunDaemon(_)
    ));
    let cli = Cli::try_parse_from(["lantern", "daemon", "--dev"]).unwrap();
    assert!(
        matches!(dispatch(cli, &UnassembledSystem, &mut out, &mut err, false).await, CliAction::RunDaemon(args) if args.dev)
    );
    let cli = Cli::try_parse_from(["lantern", "gui", "--tray", "myapp"]).unwrap();
    assert!(
        matches!(dispatch(cli, &UnassembledSystem, &mut out, &mut err, false).await, CliAction::RunGui(args) if args.tray && args.target.as_deref() == Some("myapp"))
    );
    let (code, out, err) = run(&["schema", "manifest", "--json"], "http://127.0.0.1:39699", false).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        serde_json::from_str::<Value>(&out)
            .unwrap()
            .get("properties")
            .is_some()
    );
    let (code, out, err) = run(&["completions", "bash"], "http://127.0.0.1:39699", false).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("_lantern"));
}

#[test]
fn daemon_url_uses_d21_environment_and_explicit_host_wins() {
    if let Ok(url) = std::env::var("LANTERN_DAEMON_URL") {
        assert_eq!(
            Cli::try_parse_from(["lantern", "ls"]).unwrap().host.as_deref(),
            Some(url.as_str())
        );
    }
    assert_eq!(
        Cli::try_parse_from(["lantern", "--host", "http://127.0.0.1:39698", "ls"])
            .unwrap()
            .host
            .as_deref(),
        Some("http://127.0.0.1:39698")
    );
}

#[tokio::test]
async fn multiple_target_progress_subscribes_concurrently() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _guard = NETWORK_TESTS.lock().await;
    let listener = bind();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let (_, primary) = fixture();
    let mut branch = primary.clone();
    branch.id = InstanceId::derived(&branch.project, "feature").unwrap();
    branch.checkout.branch = Some("feature".into());
    let subscriptions = Arc::new(AtomicUsize::new(0));
    let shared = subscriptions.clone();
    let server = thread::spawn(move || {
        let mut workers = vec![];
        for _ in 0..8 {
            let mut stream = accept_bounded(&listener);
            let views = vec![primary.clone(), branch.clone()];
            let shared = shared.clone();
            workers.push(thread::spawn(move || {
                let (path, _) = read_request(&mut stream);
                let (body, mime) = if path.starts_with("GET /api HTTP") {
                    (json!({"name":"Lantern","version":"0.2","api_version":1,"mcp":"/mcp","links":{}}).to_string(), "application/json")
                } else if path.starts_with("GET /api/instances") { (json!({"items":views}).to_string(), "application/json") }
                else if path.starts_with("POST /api/instances/") {
                    let suffix = if path.contains("%40feature") { "b" } else { "a" };
                    let mut value = operation("queued"); value["operation"]["id"] = json!(format!("op-{suffix}"));
                    (value.to_string(), "application/json")
                } else if path.starts_with("GET /api/events?") {
                    shared.fetch_add(1, Ordering::SeqCst);
                    let suffix = if path.contains("op=op-b") { "b" } else { "a" };
                    let event = json!({"type":"operation_progress","op":format!("op-{suffix}"),"instance":"myapp","step":format!("step-{suffix}"),"done":false,"error":null});
                    (format!("event: operation_progress\ndata: {event}\n\n"), "text/event-stream")
                } else if path.starts_with("GET /api/operations/") {
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    while shared.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline { thread::sleep(Duration::from_millis(5)); }
                    assert_eq!(shared.load(Ordering::SeqCst), 2, "both streams must start before either operation completes");
                    thread::sleep(Duration::from_millis(50));
                    let mut value = operation("succeeded")["operation"].clone();
                    value["id"] = json!(if path.contains("op-b") { "op-b" } else { "op-a" });
                    (value.to_string(), "application/json")
                } else { panic!("unexpected request: {path}") };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let (code, out, err) = run(&["on", "myapp", "myapp@feature", "--no-color"], &host, true).await;
    server.join().unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("step-a") && err.contains("step-b"), "{err}");
    assert_eq!(out, "op-a\nop-b\n");
    assert_eq!(subscriptions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn system_step_dispatch_propagates_failure_without_elevation() {
    struct FakeSystem;
    impl lantern_cli::SystemActions for FakeSystem {
        fn execute(&self, command: &lantern_cli::SystemCommand) -> Result<Value, String> {
            assert!(matches!(
                command,
                lantern_cli::SystemCommand::InitSystem { uid: Some(1234), .. }
            ));
            Ok(json!({"system_steps":[{"id":"dns", "state":"Failed", "detail":"fixture"}]}))
        }
    }
    let cli = Cli::try_parse_from(["lantern", "init", "--system-steps", "--uid", "1234"]).unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert!(matches!(
        dispatch(cli, &FakeSystem, &mut out, &mut err, false).await,
        CliAction::Exit(1)
    ));
    assert!(String::from_utf8(out).unwrap().contains("fixture"));
}

#[tokio::test]
async fn doctor_and_init_render_human_or_one_json_document() {
    struct FakeSystem;
    impl lantern_cli::SystemActions for FakeSystem {
        fn execute(&self, command: &lantern_cli::SystemCommand) -> Result<Value, String> {
            if matches!(command, lantern_cli::SystemCommand::Doctor { .. }) {
                Ok(json!({"checks":[
                    {"id":"ports.bind","title":"Port permissions","status":"fail","detail":"blocked","fix":{"description":"Allow access","command":"sudo lantern init --system-steps","needs_root":true}},
                    {"id":"session.systemd","title":"User systemd","status":"warn","detail":"degraded","fix":{"description":"Inspect failed units","command":"systemctl --user --failed","needs_root":false}},
                    {"id":"ports.free","title":"Port availability","status":"ok","detail":"free","fix":null}
                ]}))
            } else {
                Ok(
                    json!({"user_steps":[{"id":"ca","state":"Changed","detail":"created"}],"system_steps":[{"id":"ports","state":"Pending"}],"pkexec":null}),
                )
            }
        }
    }
    for args in [
        &["lantern", "doctor"][..],
        &["lantern", "doctor", "--json"][..],
        &["lantern", "init", "--yes"][..],
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let action = dispatch(cli, &FakeSystem, &mut out, &mut err, false).await;
        let text = String::from_utf8(out).unwrap();
        assert!(err.is_empty());
        if args.contains(&"doctor") {
            assert!(matches!(action, CliAction::Exit(8)));
            if args.contains(&"--json") {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["checks"].as_array().unwrap().len(), 3);
                assert_eq!(text.lines().count(), 1);
            } else {
                assert!(text.contains("PORTS\n  ✗ Port permissions: blocked"), "{text}");
                assert!(text.contains("  ✓ Port availability: free"));
                assert!(text.contains("SESSION\n  ⚠ User systemd: degraded"));
                assert!(text.contains("fix: Allow access — sudo lantern init --system-steps"));
                assert!(text.contains("systemctl --user --failed"));
                assert!(!text.trim_start().starts_with('{'));
            }
        } else {
            assert!(matches!(action, CliAction::Exit(0)));
            assert!(text.contains("User steps:\n  ca: Changed — created"), "{text}");
            assert!(text.contains("System steps:\n  ports: Pending"));
        }
    }
}

#[tokio::test]
async fn init_port_strategy_and_root_fallback_cli_are_forwarded() {
    use lantern_cli::args::PortMode;
    struct FakeSystem;
    impl lantern_cli::SystemActions for FakeSystem {
        fn execute(&self, command: &lantern_cli::SystemCommand) -> Result<Value, String> {
            match command {
                lantern_cli::SystemCommand::Init {
                    ports: PortMode::Sysctl,
                    ..
                }
                | lantern_cli::SystemCommand::Init {
                    ports: PortMode::Setcap,
                    ..
                }
                | lantern_cli::SystemCommand::InitSystem {
                    uid: None,
                    ports: PortMode::Sysctl,
                    from_package: false,
                }
                | lantern_cli::SystemCommand::InitSystem {
                    uid: None,
                    ports: PortMode::Sysctl,
                    from_package: true,
                } => (),
                other => panic!("unexpected init parameters: {other:?}"),
            }
            Ok(json!({"system_steps":[]}))
        }
    }
    for args in [
        &["lantern", "init", "--yes"][..],
        &["lantern", "init", "--yes", "--ports=setcap"][..],
        &["lantern", "init", "--system-steps"][..],
        &["lantern", "init", "--system-steps", "--from-package"][..],
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert!(matches!(
            dispatch(cli, &FakeSystem, &mut out, &mut err, false).await,
            CliAction::Exit(0)
        ));
        assert!(err.is_empty());
    }
}

#[tokio::test]
async fn doctor_warning_only_keeps_success_exit() {
    struct FakeSystem;
    impl lantern_cli::SystemActions for FakeSystem {
        fn execute(&self, _: &lantern_cli::SystemCommand) -> Result<Value, String> {
            Ok(
                json!({"checks":[{"id":"session.systemd","title":"User systemd","status":"warn","detail":"degraded","fix":{"description":"inspect units","command":"systemctl --user --failed","needs_root":false}}]}),
            )
        }
    }
    for args in [&["lantern", "doctor"][..], &["lantern", "doctor", "--json"][..]] {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert!(matches!(
            dispatch(cli, &FakeSystem, &mut out, &mut err, false).await,
            CliAction::Exit(0)
        ));
        assert!(err.is_empty());
    }
}
