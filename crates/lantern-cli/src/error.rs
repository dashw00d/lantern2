//! Stable CLI exit-code and error-envelope mapping.
use lantern_client::ClientError;
use lantern_core::api::ApiError;
use serde_json::{Value, json};

#[derive(Debug)]
pub(crate) struct Failure {
    pub code: i32,
    pub error: ApiError,
}
impl Failure {
    pub fn new(code: i32, kind: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            error: ApiError {
                code: kind.into(),
                message: message.into(),
                details: Value::Null,
            },
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(2, "bad_request", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(4, "not_found", message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(2, "not_assembled", message)
    }
    pub fn from_client(error: ClientError, host: &str) -> Self {
        match error {
            ClientError::DaemonUnreachable(_) => Self {
                code: 3,
                error: ApiError {
                    code: "daemon_unreachable".into(),
                    message: format!("can't reach the Lantern daemon at {host}"),
                    details: json!({"hint":"systemctl --user start lantern"}),
                },
            },
            ClientError::Timeout => Self::new(
                6,
                "timeout",
                "the request timed out; the operation may still be running",
            ),
            ClientError::Api { status, error } => Self {
                code: code_for_api(status, &error.code),
                error,
            },
            ClientError::Protocol(message) => Self::new(1, "protocol", message),
        }
    }
    pub fn json(&self) -> Value {
        json!({ "error": self.error, "exit_code": self.code })
    }
    pub fn human(&self) -> String {
        if self.code == 3 {
            format!(
                "error: {}\n  start it:    systemctl --user start lantern      (or: lantern daemon start)\n  not set up?  sudo lantern init\n",
                self.error.message
            )
        } else {
            format!("error: {}\n", self.error.message)
        }
    }
}
impl From<std::io::Error> for Failure {
    fn from(value: std::io::Error) -> Self {
        Self::new(1, "io_error", value.to_string())
    }
}

pub(crate) fn code_for_api(status: u16, code: &str) -> i32 {
    if code == "timeout" {
        return 6;
    }
    if code == "operation_failed" {
        return 5;
    }
    if status == 404 || code.ends_with("_not_found") || code == "not_found" || code == "branch_not_found" {
        return 4;
    }
    if status == 409
        || matches!(
            code,
            "not_managed" | "dirty_worktree" | "port_unavailable" | "operation_in_progress"
        )
    {
        return 7;
    }
    if matches!(status, 400 | 405 | 422) {
        return 2;
    }
    1
}
