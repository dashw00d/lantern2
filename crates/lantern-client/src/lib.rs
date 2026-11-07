//! Async typed client for Lantern's REST and SSE API.

use std::time::Duration;

use lantern_core::api::{API_VERSION, ApiError, ApiRoot, ErrorEnvelope};
use reqwest::{Method, RequestBuilder, Response, Url};
use serde::{Serialize, de::DeserializeOwned};

mod routes;
mod stream;
pub use routes::*;
pub use stream::{EventsOptions, StreamEvent};

/// Default local daemon address, overridden by `LANTERN_DAEMON_URL`.
pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:4777";

/// Optional explicit daemon address; otherwise the environment and default are used.
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    /// Complete daemon base URL.
    pub base_url: Option<String>,
}

/// Failure communicating with the daemon or interpreting its response.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon could not be reached.
    #[error("daemon unreachable: {0}")]
    DaemonUnreachable(String),
    /// A connection or request deadline elapsed.
    #[error("daemon request timed out")]
    Timeout,
    /// The daemon returned a structured API error.
    #[error("daemon returned HTTP {status}: {error:?}")]
    Api {
        /// HTTP status returned by the daemon.
        status: u16,
        /// Stable code, message, and details supplied by the daemon.
        error: ApiError,
    },
    /// An invalid URL, request, or unexpected response.
    #[error("daemon protocol error: {0}")]
    Protocol(String),
}

impl ClientError {
    fn from_reqwest(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else if error.is_connect() {
            Self::DaemonUnreachable(error.to_string())
        } else {
            Self::Protocol(error.to_string())
        }
    }
}

/// Cheap-to-clone daemon client.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
}
impl Client {
    /// Construct a client using the explicit address, environment, or default, in that order.
    pub fn new(cfg: ClientConfig) -> Result<Self, ClientError> {
        let address = cfg.base_url.unwrap_or_else(|| {
            std::env::var("LANTERN_DAEMON_URL").unwrap_or_else(|_| DEFAULT_DAEMON_URL.into())
        });
        let base =
            Url::parse(&address).map_err(|e| ClientError::Protocol(format!("invalid daemon URL: {e}")))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::Protocol(
                "daemon URL must be an HTTP(S) address without credentials, query, or fragment".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ClientError::from_reqwest)?;
        Ok(Self { http, base })
    }
    /// Perform a one-second liveness probe, rejecting unsupported newer API versions.
    pub async fn ping(&self) -> Result<ApiRoot, ClientError> {
        let root: ApiRoot = self
            .json(self.request(Method::GET, &["api"]), Duration::from_secs(1))
            .await?;
        if root.api_version > API_VERSION {
            return Err(ClientError::Protocol(format!(
                "daemon API version {} is newer than supported version {API_VERSION}",
                root.api_version
            )));
        }
        Ok(root)
    }
    fn url(&self, segments: &[&str]) -> Url {
        // Each component is encoded independently; slash, @, ?, # and non-ASCII bytes cannot escape a segment.
        let mut url = self.base.clone();
        let mut path = self.base.path().trim_end_matches('/').to_string();
        for segment in segments {
            path.push('/');
            for byte in segment.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                    path.push(char::from(byte));
                } else {
                    use std::fmt::Write;
                    write!(&mut path, "%{byte:02X}").expect("writing to String cannot fail");
                }
            }
        }
        url.set_path(&path);
        url
    }
    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        self.http.request(method, self.url(segments))
    }
    async fn response(
        &self,
        req: RequestBuilder,
        timeout: Option<Duration>,
    ) -> Result<Response, ClientError> {
        let req = if let Some(timeout) = timeout {
            req.timeout(timeout)
        } else {
            req
        };
        let res = req.send().await.map_err(ClientError::from_reqwest)?;
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status().as_u16();
        let bytes = res.bytes().await.map_err(ClientError::from_reqwest)?;
        let envelope: ErrorEnvelope = serde_json::from_slice(&bytes).map_err(|e| {
            ClientError::Protocol(format!("HTTP {status} returned an invalid error envelope: {e}"))
        })?;
        Err(ClientError::Api {
            status,
            error: envelope.error,
        })
    }
    async fn json<T: DeserializeOwned>(
        &self,
        req: RequestBuilder,
        timeout: Duration,
    ) -> Result<T, ClientError> {
        self.response(req, Some(timeout))
            .await?
            .json()
            .await
            .map_err(ClientError::from_reqwest)
    }
    async fn empty(&self, req: RequestBuilder) -> Result<(), ClientError> {
        self.response(req, Some(Duration::from_secs(10))).await?;
        Ok(())
    }
}

/// Whether to wait for a long operation and, optionally, for how long.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wait {
    /// Wait for completion rather than returning the current operation.
    pub wait: bool,
    /// Wait duration; the daemon defaults to 60 seconds and accepts at most 600.
    pub timeout: Option<Duration>,
}
impl Wait {
    fn apply(self, req: RequestBuilder) -> Result<(RequestBuilder, Duration), ClientError> {
        let timeout = self
            .timeout
            .map(|d| d.as_secs().saturating_add(u64::from(d.subsec_nanos() != 0)));
        if timeout.is_some_and(|n| n > 600 || n == 0) {
            return Err(ClientError::Protocol(
                "wait timeout must be between 1 and 600 seconds".into(),
            ));
        }
        let mut req = req.query(&[("wait", self.wait.to_string())]);
        if let Some(seconds) = timeout {
            req = req.query(&[("timeout", seconds)]);
        }
        let deadline = if self.wait {
            Duration::from_secs(timeout.unwrap_or(60) + 5)
        } else {
            Duration::from_secs(10)
        };
        Ok((req, deadline))
    }
}

fn append_query<T: Serialize + ?Sized>(req: RequestBuilder, query: &T) -> RequestBuilder {
    req.query(query)
}
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
