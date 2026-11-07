use super::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt::Debug;

fn round<T: DeserializeOwned + Serialize + PartialEq + Debug>(value: Value) -> T {
    let original: T = serde_json::from_value(value).unwrap();
    let encoded = serde_json::to_value(&original).unwrap();
    let decoded: T = serde_json::from_value(encoded).unwrap();
    assert_eq!(original, decoded);
    original
}
fn timestamp() -> &'static str {
    "2026-11-06T17:52:03.412Z"
}
fn manifest() -> Value {
    json!({"id":"myapp", "serve":"proxy", "run_cmd":"npm run dev", "diagnostics":{"errors":0,"warnings":1,"info":2},"manifest_rev":"abc"})
}
fn project() -> Value {
    json!({"id":"myapp","aliases":[],"name":"My App","root":"/tmp/myapp", "repo":null,
        "kind":"project","enabled":true,"tags":[],"registered_at":timestamp(),"source":"manifest"})
}
fn summary() -> Value {
    let mut p = project();
    p.as_object_mut().unwrap().extend(
        json!({"primary":"myapp","status":"running","url":"https://myapp.glow",
        "port":29000,"instance_count":1,"docs_count":0,"endpoints_count":0,"routing":null})
        .as_object()
        .unwrap()
        .clone(),
    );
    p
}
fn instance() -> Value {
    json!({"id":"myapp", "project":"myapp", "checkout":{"path":"/tmp/myapp","git_root":null,
        "kind":"primary","branch":null,"head":null,"dirty":null,"locked":null,"missing":false},
        "domain":"myapp.glow","url":"https://myapp.glow","pinned":false,"adopt":false,"visible":true,"setup_stale":false,
        "runtime":{"status":"stopped","port":null,"unit":null,"main_pid":null,"started_at":null,"last_exit":null,"restarts":0,
            "next_retry_at":null,"health":{"state":"unknown","checked_at":null,"latency_ms":null,"consecutive_failures":0,"last_error":null,"recent":[]},
            "message":null,"issue":null,"upstream":null,"external":null},"manifest":manifest()})
}
fn operation() -> Value {
    json!({"id":"op_123","kind":"start","instance":"myapp","status":"running","noop":false,
        "step":null,"steps":[],"created_at":timestamp(),"finished_at":null,"error":null,"result":null})
}
fn health() -> Value {
    json!({"ok":true,"checked_at":timestamp(),"checks":[]})
}
fn service() -> Value {
    json!({"name":"php_fpm","state":"stopped","mode":"user","ports":{},"ui_url":null,"unit":null,"controllable":true,"detail":null})
}

#[test]
fn views_and_requests_round_trip() {
    round::<List<String>>(json!({"items":["a"]}));
    round::<ErrorEnvelope>(
        json!({"error":{"code":"custom_future_code","message":"bad","details":{"field":"x"}}}),
    );
    round::<ApiError>(json!({"code":"bad_request","message":"bad"}));
    round::<DiagnosticCounts>(json!({"errors":0,"warnings":1,"info":2}));
    round::<ManifestSummary>(manifest());
    round::<RoutingMeta>(
        json!({"triggers":[],"risk":"high","requires_confirmation":false,"max_concurrent":null,"agents":[]}),
    );
    round::<ProjectSummary>(summary());
    let mut detail = summary();
    detail.as_object_mut().unwrap().extend(
        json!({"instances":[instance()],"depends_on":[],"manifest":manifest()})
            .as_object()
            .unwrap()
            .clone(),
    );
    round::<ProjectDetail>(detail);
    round::<ProjectBrief>(
        json!({"name":"My App","id":"myapp","path":"/tmp/myapp","domain":"myapp.glow","url":"https://myapp.glow","kind":"project","status":"running","port":29000,"description":null,"tags":[],"enabled":true,"instances":1,"branches":[],"docs_count":0,"endpoints_count":0}),
    );
    round::<InstanceView>(instance());
    round::<Suggestion>(
        json!({"path":"/tmp/myapp","proposed_id":"myapp","framework":"nextjs","confidence":"high"}),
    );
    round::<RegisterProjectRequest>(
        json!({"path":"/tmp/myapp","id":null,"name":null,"enabled":null,"overrides":null}),
    );
    round::<PatchProjectRequest>(
        json!({"id":"renamed","name":null,"enabled":null,"tags":null,"overrides":null}),
    );
    let p = round::<UnregisterParams>(json!({}));
    assert!(p.stop && !p.remove_worktrees);
    round::<ScanRequest>(json!({"roots":null}));
    round::<ScanSkip>(json!({"path":"/tmp/myapp","reason":"already registered"}));
    round::<ScanReport>(json!({"registered":[summary()],"already_known":0,"skipped":[]}));
    round::<ReloadRequest>(json!({}));
    for target in [
        json!({"branch":"feature/x"}),
        json!({"new_branch":"feature/x","base":"main"}),
        json!({"detached":"abc123"}),
        json!({"pr":123}),
    ] {
        round::<RunTarget>(target.clone());
        let req = round::<CreateInstanceRequest>(json!({"target": target}));
        assert!(req.fetch && req.start);
    }
    round::<PatchInstanceRequest>(json!({"pinned":true}));
    round::<PinRequest>(json!({}));
    round::<UnpinRequest>(json!({}));
    round::<RenameInstanceRequest>(json!({"slug":"new-name"}));
    round::<PrepareRequest>(json!({}));
    round::<AdoptRequest>(json!({"port":29000}));
    round::<DismissSuggestionRequest>(json!({"path":"/tmp/myapp"}));
    round::<ImportV1Request>(json!({"from":null}));
    round::<SchemaRequest>(json!({"name":"manifest"}));
    round::<StopAllRequest>(json!({}));
    round::<WaitParams>(json!({"wait":true,"timeout":10}));
    round::<RemoveInstanceParams>(json!({}));
    round::<BranchInfo>(
        json!({"name":"main","remote":null,"oid":"abc","upstream":null,"ahead":0,"behind":0,"gone":false,"committed_at":0,"checked_out_at":null}),
    );
    for value in [
        json!("not_managed"),
        json!("running"),
        json!({"locked":"reason"}),
        json!({"dirty":2}),
        json!({"unpushed":3}),
        json!({"orphaned":4}),
    ] {
        round::<Blocker>(value);
    }
    round::<RemovalCheck>(
        json!({"blockers":[],"dirty":[],"unpushed":0,"orphaned":0,"has_submodules":false,"worktree_env":{},"branch":null}),
    );
}

#[test]
fn run_branch_shorthand_and_canonical_wire_forms() {
    let a: CreateInstanceRequest = serde_json::from_value(json!({"branch":"feature/x"})).unwrap();
    assert_eq!(a.target, RunTarget::Branch("feature/x".into()));
    assert!(a.fetch && a.start);
    let b: CreateInstanceRequest =
        serde_json::from_value(json!({"branch":"feature/y","base":"main","start":false})).unwrap();
    assert_eq!(
        b.target,
        RunTarget::NewBranch {
            name: "feature/y".into(),
            base: Some("main".into())
        }
    );
    assert_eq!(
        serde_json::to_value(&b).unwrap()["target"],
        json!({"new_branch":"feature/y","base":"main"})
    );
    assert!(
        serde_json::from_value::<CreateInstanceRequest>(json!({"target":{"pr":4},"branch":"bad"})).is_err()
    );
}

#[test]
fn operations_intel_system_and_misc_round_trip() {
    round::<OperationId>(json!("op_123"));
    round::<OperationStep>(json!({"name":"start","status":"running","started_at":null,"finished_at":null}));
    round::<Operation>(operation());
    round::<OperationResponse>(
        json!({"operation":operation(),"instance":null,"created":null,"forgotten":true}),
    );
    round::<OperationsResponse>(json!({"operations":[operation()]}));
    round::<LogParams>(json!({"tail":10,"since":null,"stream":"setup","grep":null,"format":"json"}));
    round::<LogPage>(
        json!({"instance":"myapp","lines":[{"instance":"myapp","stream":"stdout","line":"ok","ts":timestamp()}],"buffered":1,"truncated":false}),
    );
    round::<PortEntry>(
        json!({"port":29000,"status":"running","health_status":"healthy","url":"https://myapp.glow"}),
    );
    round::<PortConflict>(json!({"instance":"myapp","holder":{"port":29000}}));
    round::<PortsView>(json!({"instances":{},"unmanaged":[],"conflicts":[]}));
    let doc =
        json!({"path":"README.md","kind":"readme","source":"manual","exists":true,"size":10,"mtime":null});
    round::<DocEntry>(doc.clone());
    round::<DocContent>(
        json!({"path":"README.md","content":"hello","offset":0,"total_chars":5,"truncated":false}),
    );
    round::<ParamHint>(json!({"name":"id","in":"path","required":true}));
    round::<EndpointEntry>(
        json!({"method":"GET","path":"/","description":null,"category":null,"risk":"low","body_hint":null,"params":[],"source":"discovered"}),
    );
    let docs = json!({"enabled":true,"count":1,"source_count":1,"sources":["README.md"],"skipped_large":0,"truncated":false});
    let api = json!({"enabled":true,"count":0,"source_count":0,"sources":[],"errors":[]});
    round::<DocsMeta>(docs.clone());
    round::<ApiMeta>(api.clone());
    round::<IntelError>(json!({"source":"/api","error":"not found"}));
    let discovery = json!({"refreshed_at":null,"docs":docs,"api":api});
    round::<DiscoveryMeta>(discovery.clone());
    round::<IntelSnapshot>(
        json!({"instance":"myapp","refreshed_at":null,"stale":false,"docs":[doc],"endpoints":[],"discovery":discovery}),
    );
    round::<IntelRefreshReport>(json!({"refreshed":1,"errors":[],"took_ms":2}));
    round::<DependencyEntry>(json!({"depends_on":[],"depended_by":[]}));
    round::<DependencyGraph>(json!({"myapp":{"depends_on":[],"depended_by":[]}}));
    round::<ApiRoot>(json!({"name":"lantern","version":"2","api_version":1,"mcp":"/mcp","links":{}}));
    round::<SystemInfo>(
        json!({"version":"2","api_version":1,"pid":12,"boot_id":"id","started_at":timestamp(),"listen":"127.0.0.1:4777","tld":"glow","paths":{}}),
    );
    round::<ReconcileResponse>(json!({"reconciled":true}));
    round::<SettingsPatched>(json!({"settings":{},"restart_required":[]}));
    round::<Fix>(json!({"description":"fix","command":"run","needs_root":false}));
    round::<Check>(json!({"id":"daemon.running","title":"Daemon","status":"ok","detail":"ok","fix":null}));
    round::<SystemHealth>(health());
    round::<ServiceStatus>(service());
    round::<EventParams>(json!({"state":false,"logs":["myapp"],"tail":2,"op":"op_123"}));
}

#[test]
fn all_enum_values_round_trip_with_snake_case() {
    for value in ["low", "medium", "high", "critical", "unknown"] {
        round::<Risk>(json!(value));
    }
    for value in ["high", "medium", "low"] {
        round::<Confidence>(json!(value));
    }
    for value in [
        "start",
        "stop",
        "restart",
        "prepare",
        "create_instance",
        "remove_instance",
        "stop_all",
        "profile_activate",
        "api_call",
    ] {
        round::<OperationKind>(json!(value));
    }
    for value in ["queued", "running", "succeeded", "failed", "cancelled"] {
        round::<OperationStatus>(json!(value));
    }
    for value in ["json", "text"] {
        round::<LogFormat>(json!(value));
    }
    for value in ["manual", "discovered"] {
        round::<IntelSource>(json!(value));
    }
    for value in ["ok", "warn", "fail", "skipped"] {
        round::<CheckStatus>(json!(value));
    }
    for value in ["mailpit", "redis", "postgres", "php_fpm"] {
        round::<ServiceName>(json!(value));
    }
    for value in ["running", "stopped", "starting", "failed", "not_installed"] {
        round::<ServiceState>(json!(value));
    }
    for value in ["user", "system"] {
        round::<ServiceMode>(json!(value));
    }
}

#[test]
fn events_have_matching_type_and_sse_names() {
    let inputs = [
        json!({"type":"project_updated","project":summary()}),
        json!({"type":"project_removed","id":"myapp"}),
        json!({"type":"instance_updated","instance":instance()}),
        json!({"type":"instance_removed","id":"myapp"}),
        json!({"type":"listeners_changed","seq":7,"listeners":[]}),
        json!({"type":"suggestions_changed","suggestions":[]}),
        json!({"type":"service_updated","service":service()}),
        json!({"type":"system_health","health":health()}),
        json!({"type":"operation_progress","op":"op_123","instance":"myapp","step":"start","done":true,"error":null}),
    ];
    for input in inputs {
        let event: Event = round(input.clone());
        let wire: WireEvent = event.clone().into();
        assert_eq!(event.event_name(), wire.event_name());
        assert_eq!(serde_json::to_value(&wire).unwrap()["type"], wire.event_name());
        assert_eq!(round::<WireEvent>(input), wire);
    }
    for input in [
        json!({"type":"hello","api_version":1,"daemon_version":"2","boot_id":"id","now":timestamp()}),
        json!({"type":"snapshot_complete"}),
        json!({"type":"resync","reason":"lagged","skipped":1}),
        json!({"type":"log","instance":"myapp","stream":"stdout","line":"hello","ts":timestamp()}),
    ] {
        let wire = round::<WireEvent>(input);
        assert_eq!(serde_json::to_value(&wire).unwrap()["type"], wire.event_name());
    }
}
