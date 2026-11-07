//! HTTP status and structured error conversion.

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use lantern_core::api::{ApiError, ErrorEnvelope};
use serde_json::{Value, json};

/// An API failure, represented by the shared wire envelope.
#[derive(Debug)]
pub struct HttpError(pub ApiError, pub Option<StatusCode>);

impl HttpError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self(
            ApiError {
                code: code.into(),
                message: message.into(),
                details: json!({}),
            },
            None,
        )
    }
    pub(crate) fn status(code: &str) -> StatusCode {
        match code {
            "bad_request" | "invalid_json" => StatusCode::BAD_REQUEST,
            "not_found"
            | "project_not_found"
            | "instance_not_found"
            | "service_not_found"
            | "operation_not_found"
            | "doc_not_found" => StatusCode::NOT_FOUND,
            "forbidden_host" | "forbidden_origin" | "forbidden" => StatusCode::FORBIDDEN,
            "already_exists"
            | "operation_in_progress"
            | "cannot_remove_primary"
            | "dirty_worktree"
            | "port_unavailable"
            | "builtin_template"
            | "not_controllable" => StatusCode::CONFLICT,
            "reserved_id" | "validation_failed" | "needs_config" | "branch_invalid" => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            "unsupported_media_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "payload_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            "method_not_allowed" => StatusCode::METHOD_NOT_ALLOWED,
            "timeout" => StatusCode::GATEWAY_TIMEOUT,
            "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    pub(crate) fn operation(mut error: ApiError, id: &str) -> Self {
        if !error.details.is_object() {
            error.details = Value::Object(Default::default());
        }
        if let Some(details) = error.details.as_object_mut() {
            details.insert("operation_id".into(), Value::String(id.into()));
        }
        Self(error, None)
    }
}
impl From<ApiError> for HttpError {
    fn from(value: ApiError) -> Self {
        Self(value, None)
    }
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (
            self.1.unwrap_or_else(|| Self::status(&self.0.code)),
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            Json(ErrorEnvelope { error: self.0 }),
        )
            .into_response()
    }
}
