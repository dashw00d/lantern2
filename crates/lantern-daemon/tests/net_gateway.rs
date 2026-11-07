use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use lantern_core::{ids::InstanceId, model::InstanceStatus};
use lantern_daemon::net::gateway::{
    GatewayBackend, GatewayInstance, GatewayStartFuture, GatewayState, router,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

#[derive(Debug, Default)]
struct Backend {
    starts: AtomicUsize,
}
impl GatewayBackend for Backend {
    fn known_hosts(&self) -> Vec<String> {
        vec!["app.glow".into(), "lantern.glow".into()]
    }
    fn instance_for_host(&self, host: &str) -> Option<GatewayInstance> {
        (host == "app.glow").then(|| GatewayInstance {
            id: InstanceId::parse("app").unwrap(),
            status: InstanceStatus::Stopped,
            message: Some("<script>alert('x')</script> & broken".into()),
            port: None,
        })
    }
    fn start<'a>(&'a self, _: &'a InstanceId) -> GatewayStartFuture<'a> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
fn state(backend: Arc<Backend>) -> GatewayState {
    GatewayState {
        backend,
        daemon_port: 39720,
        https_port: 39721,
        root_cert: PathBuf::from("/missing/test.crt"),
    }
}
fn req(method: &str, path: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:39720")
        .header("x-forwarded-host", "app.glow:39721")
}
#[tokio::test]
async fn net_gateway_guard_and_escaped_nonce_pages() {
    let backend = Arc::new(Backend::default());
    let app = router(state(backend.clone()));
    let invalid = Request::builder()
        .uri("/__lantern/ping")
        .header("host", "attacker.test")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(invalid).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let request = req("GET", "/__lantern/page").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let csp = response
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let html = String::from_utf8(to_bytes(response.into_body(), 65536).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; broken"));
    assert!(!html.contains("<script>alert"));
    assert!(csp.contains("'nonce-"));
    assert!(html.contains("nonce=\""));
    let response = app
        .clone()
        .oneshot(req("GET", "/__lantern/page").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(
        csp,
        response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
    );
    for request in [
        req("POST", "/__lantern/start").body(Body::empty()).unwrap(),
        req("POST", "/__lantern/start")
            .header("origin", "https://evil.test")
            .body(Body::empty())
            .unwrap(),
        req("POST", "/__lantern/start")
            .header("origin", "https://app.glow:39721")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .unwrap(),
        req("POST", "/__lantern/start")
            .header("origin", "https://app.glow:39721")
            .header("x-forwarded-host", "evil.glow:39721")
            .body(Body::empty())
            .unwrap(),
    ] {
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let allowed = req("POST", "/__lantern/start")
        .header("origin", "https://app.glow:39721")
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(allowed).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
    let default_port = router(GatewayState {
        backend: backend.clone(),
        daemon_port: 39720,
        https_port: 443,
        root_cert: PathBuf::from("/missing/test.crt"),
    });
    let bad_port = Request::builder()
        .method("POST")
        .uri("/__lantern/start")
        .header("host", "127.0.0.1:39720")
        .header("x-forwarded-host", "app.glow:99999")
        .header("origin", "https://app.glow:99999")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        default_port.oneshot(bad_port).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let unknown = Request::builder()
        .uri("/__lantern/page")
        .header("host", "localhost:39720")
        .header("x-forwarded-host", "other.glow:39721")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(unknown).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}
