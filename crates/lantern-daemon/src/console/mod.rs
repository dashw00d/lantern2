//! The control center: the web UI (built from `ui/`, embedded at compile time) and the
//! command launcher API. Mounted behind the same Host/Origin guard as the REST API.

pub mod launchers;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use include_dir::{Dir, include_dir};
use launchers::{LauncherError, LauncherInput, Launchers};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

static UI: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

/// Routes for the UI and `/api/launchers`. Callers add the API guard.
pub fn router(launchers: Arc<Launchers>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*file}", get(asset))
        .route("/favicon.svg", get(|| async { file("favicon.svg") }))
        .route("/api/launchers", get(list).post(create))
        .route("/api/launchers/{id}", axum::routing::put(update).delete(delete))
        .route("/api/launchers/{id}/start", post(start))
        .route("/api/launchers/{id}/stop", post(stop))
        .route("/api/launchers/{id}/logs", get(logs))
        .with_state(launchers)
}

async fn index() -> Response {
    file("index.html")
}

async fn asset(Path(path): Path<String>) -> Response {
    file(&format!("assets/{path}"))
}

fn file(path: &str) -> Response {
    let Some(f) = UI.get_file(path) else {
        if path == "index.html" {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                "<!doctype html><title>Lantern</title><p>The control center was not built. Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code>, then rebuild Lantern.",
            )
                .into_response();
        }
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    };
    let mut response = f.contents().into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
}

impl IntoResponse for LauncherError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

async fn list(State(l): State<Arc<Launchers>>) -> Response {
    Json(json!({"items": l.list().await})).into_response()
}
async fn create(State(l): State<Arc<Launchers>>, Json(input): Json<LauncherInput>) -> Response {
    match l.create(input).await {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(e) => e.into_response(),
    }
}
async fn update(
    State(l): State<Arc<Launchers>>,
    Path(id): Path<String>,
    Json(input): Json<LauncherInput>,
) -> Response {
    match l.update(&id, input).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}
async fn delete(State(l): State<Arc<Launchers>>, Path(id): Path<String>) -> Response {
    match l.delete(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}
async fn start(State(l): State<Arc<Launchers>>, Path(id): Path<String>) -> Response {
    match l.start(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}
async fn stop(State(l): State<Arc<Launchers>>, Path(id): Path<String>) -> Response {
    match l.stop(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}
#[derive(Deserialize)]
struct Tail {
    tail: Option<usize>,
}
async fn logs(State(l): State<Arc<Launchers>>, Path(id): Path<String>, Query(q): Query<Tail>) -> Response {
    match l.logs(&id, q.tail.unwrap_or(300)).await {
        Ok(lines) => Json(json!({"lines": lines})).into_response(),
        Err(e) => e.into_response(),
    }
}
