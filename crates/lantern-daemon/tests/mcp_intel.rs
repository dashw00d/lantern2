use lantern_core::{
    detect::Detection,
    ids::InstanceId,
    manifest::{self, ResolveInput, ResolvedManifest, TemplateDefinition, TemplateLookup, raw::RawManifest},
    settings::Settings,
};
use lantern_daemon::{
    intel::{Intel, IntelInput},
    mcp::upstream::{UpstreamPolicy, default_client},
};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;

struct EmptyTemplates;
impl TemplateLookup for EmptyTemplates {
    fn get(&self, _: &str) -> Option<TemplateDefinition> {
        None
    }
}
fn manifest(yaml: &str) -> Arc<ResolvedManifest> {
    let parsed = manifest::parse_manifest(yaml, "lantern.yaml");
    Arc::new(manifest::resolve(ResolveInput {
        dir_name: "app",
        is_primary: true,
        primary: Some(&parsed),
        checkout: None,
        overrides: &RawManifest::default(),
        templates: &EmptyTemplates,
        detection: &Detection::default(),
        settings: &Settings::default(),
    }))
}
fn input(root: &TempDir, id: &str, yaml: &str, base_url: Option<String>, running: bool) -> IntelInput {
    IntelInput {
        instance: InstanceId::parse(id).unwrap(),
        checkout: root.path().to_path_buf(),
        manifest: manifest(yaml),
        base_url,
        running,
    }
}
fn intel(path: PathBuf) -> Intel {
    Intel::new(path, UpstreamPolicy::default(), default_client())
}
#[tokio::test]
async fn mcp_intel_docs_containment_text_paging_and_cache() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("docs")).unwrap();
    std::fs::create_dir(root.path().join("node_modules")).unwrap();
    std::fs::write(root.path().join("README.md"), "alpha βeta").unwrap();
    std::fs::write(root.path().join("docs/guide.md"), "guide").unwrap();
    std::fs::write(root.path().join("docs/binary.md"), b"a\0b").unwrap();
    std::fs::write(root.path().join("docs/large.md"), vec![b'x'; 200]).unwrap();
    std::fs::write(root.path().join("node_modules/README.md"), "ignored").unwrap();
    std::fs::write(other.path().join("secret.md"), "secret").unwrap();
    symlink(other.path().join("secret.md"), root.path().join("docs/escape.md")).unwrap();
    let cache = root.path().join("state/intel-cache.json");
    let service = intel(cache.clone());
    let id = InstanceId::parse("app").unwrap();
    let yaml = "id: app\ndocs:\n  - docs/escape.md\n  - ../secret.md\ndocs_auto:\n  max_bytes: 100\n";
    let snapshot = service
        .refresh(input(&root, "app", yaml, None, false))
        .await
        .unwrap();
    assert!(snapshot.docs.iter().any(|d| d.path == "README.md"));
    assert!(snapshot.docs.iter().any(|d| d.path == "docs/guide.md"));
    assert!(
        !snapshot
            .docs
            .iter()
            .any(|d| d.path == "docs/binary.md" || d.path == "docs/large.md" || d.path == "../secret.md")
    );
    assert!(snapshot.discovery.docs.skipped_large >= 1);
    assert!(service.read_doc(&id, "docs/escape.md", 0, 100).await.is_err());
    assert!(service.read_doc(&id, "../secret.md", 0, 100).await.is_err());
    let page = service.read_doc(&id, "README.md", 6, 2).await.unwrap();
    assert_eq!(page.content, "βe");
    assert!(page.truncated);
    assert!(cache.exists());
    let restored = intel(cache);
    assert_eq!(restored.load_cache(std::slice::from_ref(&id)).await.unwrap(), 1);
    assert!(restored.snapshot(&id).await.unwrap().stale);
    assert!(restored.read_doc(&id, "README.md", 0, 20).await.is_err());
}
#[tokio::test]
async fn mcp_intel_file_openapi_and_instances() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("api.yaml"),"openapi: 3.0.0\npaths:\n  /items/{id}:\n    get:\n      summary: List items\n      tags: [Items]\n    delete:\n      x-risk: critical\n").unwrap();
    let service = intel(root.path().join("cache.json"));
    let yaml = "id: app\napi_auto:\n  enabled: true\n  sources: [api.yaml]\n";
    let primary = service
        .refresh(input(&root, "app", yaml, None, false))
        .await
        .unwrap();
    assert_eq!(primary.endpoints.len(), 2);
    assert_eq!(primary.endpoints[0].path, "/items/{id}");
    assert!(
        primary
            .endpoints
            .iter()
            .any(|e| e.method == "DELETE" && matches!(e.risk, lantern_core::api::Risk::Critical))
    );
    let second = service
        .refresh(input(&root, "app@feature", "id: app\n", None, false))
        .await
        .unwrap();
    assert!(second.endpoints.is_empty());
    assert_eq!(
        service
            .snapshot(&InstanceId::parse("app").unwrap())
            .await
            .unwrap()
            .endpoints
            .len(),
        2
    );
}
#[tokio::test]
#[ignore = "real system: binds a loopback TCP socket in the mcp lane range"]
async fn mcp_intel_openapi_http_fetch() {
    let mut listener = None;
    for port in 39800..39900 {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some(l);
            break;
        }
    }
    let listener = listener.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/openapi.json",
        axum::routing::get(|| async {
            axum::Json(
                serde_json::json!({"openapi":"3.0.0","paths":{"/greet":{"get":{"summary":"Say hello"}}}}),
            )
        }),
    );
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
    });
    let root = TempDir::new().unwrap();
    let service = intel(root.path().join("cache.json"));
    let snapshot = service
        .refresh(input(
            &root,
            "app",
            "id: app\napi_auto:\n  enabled: true\n",
            Some(format!("http://{addr}")),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.endpoints.len(), 1);
    assert_eq!(snapshot.endpoints[0].path, "/greet");
    assert!(snapshot.discovery.api.errors.is_empty());
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn mcp_intel_discovery_rejects_symlinked_scan_root() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.md"), "must not appear").unwrap();
    symlink(outside.path(), root.path().join("docs")).unwrap();
    let service = intel(root.path().join("cache.json"));
    let snapshot = service
        .refresh(input(&root, "app", "id: app\n", None, false))
        .await
        .unwrap();
    assert!(!snapshot.docs.iter().any(|d| d.path.contains("secret")));
}
