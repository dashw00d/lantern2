//! SSE frame parsing and reconnect policy.

use std::{pin::Pin, time::Duration};

use futures_util::{Stream, StreamExt};
use lantern_core::{
    api::{API_VERSION, OperationId, WireEvent},
    ids::InstanceId,
};
use reqwest::Method;
use tokio::time::Instant;

use crate::{Client, ClientError};

/// Options for an SSE subscription.
#[derive(Debug, Clone)]
pub struct EventsOptions {
    /// Include the initial state snapshot and subsequent state events.
    pub state: bool,
    /// Instance ids whose logs are included (at most eight).
    pub logs: Vec<InstanceId>,
    /// Backlog lines per log stream (at most 1000).
    pub tail: usize,
    /// Operation progress to include in a log-only stream.
    pub op: Option<OperationId>,
    /// Reconnect after a clean close, resync, or failure.
    pub reconnect: bool,
}
impl Default for EventsOptions {
    fn default() -> Self {
        Self {
            state: true,
            logs: Vec::new(),
            tail: 100,
            op: None,
            reconnect: true,
        }
    }
}

/// Connection lifecycle and decoded SSE messages.
#[allow(clippy::large_enum_variant)] // Preserve the documented Event(WireEvent) public contract.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A connection attempt is beginning.
    Connecting {
        /// Zero-based retry attempt.
        attempt: u32,
    },
    /// HTTP connection established; the hello event follows.
    Connected,
    /// A decoded snapshot, state, log, or progress event.
    Event(WireEvent),
    /// An error ended the connection; reconnecting clients will wait before retrying.
    Lost {
        /// Explanation of the failure.
        error: String,
        /// Whether the daemon could not be reached.
        daemon_unreachable: bool,
        /// Zero-based failed attempt.
        attempt: u32,
        /// Delay before the next attempt, or zero without reconnection.
        retry_in: Duration,
    },
}

impl Client {
    /// Stream lifecycle notifications and events, reconnecting when requested.
    ///
    /// A resync is consumed and ends the current connection; a reconnect starts a fresh snapshot.
    /// Unknown event types are skipped. Dropping the stream stops further reconnects.
    pub fn events(
        &self,
        options: EventsOptions,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let client = self.clone();
        Box::pin(async_stream::stream! {
            let mut attempt = 0u32;
            loop {
                yield StreamEvent::Connecting { attempt };
                let mut req = client.request(Method::GET, &["api", "events"]).header(reqwest::header::ACCEPT, "text/event-stream");
                req = req.query(&[("state", options.state.to_string()), ("tail", options.tail.to_string())]);
                if !options.logs.is_empty() {
                    let logs = options.logs.iter().map(InstanceId::as_str).collect::<Vec<_>>().join(",");
                    req = req.query(&[("logs", logs)]);
                }
                if let Some(op) = &options.op { req = req.query(&[("op", &op.0)]); }
                let invalid_options = options.logs.len() > 8 || options.tail > 1000;
                let opened = if invalid_options {
                    Err(ClientError::Protocol("events supports at most 8 log streams and tail <= 1000".into()))
                } else { client.response(req, None).await };
                match opened {
                    Err(error) => {
                        let retry_in = if options.reconnect && !invalid_options { backoff(attempt) } else { Duration::ZERO };
                        let daemon_unreachable = matches!(error, ClientError::DaemonUnreachable(_));
                        yield StreamEvent::Lost { error: error.to_string(), daemon_unreachable, attempt, retry_in };
                        if !options.reconnect || invalid_options { break; }
                        tokio::time::sleep(retry_in).await;
                        attempt = attempt.saturating_add(1);
                    }
                    Ok(response) => {
                        yield StreamEvent::Connected;
                        let started = Instant::now();
                        let mut bytes = response.bytes_stream();
                        let mut parser = Parser::default();
                        let mut failure: Option<String> = None;
                        let mut resync = false;
                        let mut fatal = false;
                        loop {
                            match tokio::time::timeout(Duration::from_secs(40), bytes.next()).await {
                                Err(_) => { failure = Some("SSE stream idle for 40 seconds".into()); break; }
                                Ok(None) => break,
                                Ok(Some(Err(error))) => { failure = Some(error.to_string()); break; }
                                Ok(Some(Ok(chunk))) => {
                                    match parser.feed(&chunk) {
                                        Err(error) => { failure = Some(error); break; }
                                        Ok(frames) => {
                                            for frame in frames {
                                                match decode(frame) {
                                                    Err(error) => { failure = Some(error); break; }
                                                    Ok(None) => {},
                                                    Ok(Some(WireEvent::Resync { .. })) => { resync = true; break; }
                                                    Ok(Some(WireEvent::Hello { api_version, .. })) if api_version > API_VERSION => {
                                                        failure = Some(format!("daemon API version {api_version} is newer than supported version {API_VERSION}"));
                                                        fatal = true;
                                                        break;
                                                    }
                                                    Ok(Some(event)) => yield StreamEvent::Event(event),
                                                }
                                            }
                                            if failure.is_some() || resync { break; }
                                        }
                                    }
                                }
                            }
                        }
                        if started.elapsed() >= Duration::from_secs(10) { attempt = 0; }
                        if let Some(error) = failure {
                            let retry_in = if options.reconnect && !fatal { backoff(attempt) } else { Duration::ZERO };
                            yield StreamEvent::Lost { error, daemon_unreachable: false, attempt, retry_in };
                            if !options.reconnect || fatal { break; }
                            tokio::time::sleep(retry_in).await;
                            attempt = attempt.saturating_add(1);
                        } else if !options.reconnect { break; }
                        // A clean close or resync reconnects immediately without backoff.
                    }
                }
            }
        })
    }

    /// Open a single subscription which ends on close, failure, or resync.
    pub fn events_once(
        &self,
        mut options: EventsOptions,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        options.reconnect = false;
        self.events(options)
    }
}

fn backoff(attempt: u32) -> Duration {
    let secs = match attempt {
        0 => 0.5,
        1 => 1.0,
        2 => 2.0,
        3 => 4.0,
        4 => 8.0,
        _ => 15.0,
    };
    // Jitter is independent for each failed connection; bounds are exactly +/- 20%.
    Duration::from_secs_f64(secs * (0.8 + rand::random::<f64>() * 0.4))
}

#[derive(Debug)]
struct Frame {
    event: String,
    data: String,
}
#[derive(Debug, Default)]
struct Parser {
    pending: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    skip_lf: bool,
}
impl Parser {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        const MAX_FRAME: usize = 1024 * 1024;
        let bytes = if self.skip_lf && !bytes.is_empty() {
            self.skip_lf = false;
            if bytes[0] == b'\n' { &bytes[1..] } else { bytes }
        } else {
            bytes
        };
        self.pending.extend_from_slice(bytes);
        if self.pending.len() + self.data.len() + self.event.len() > MAX_FRAME {
            return Err("SSE frame exceeds 1 MiB".into());
        }
        let mut frames = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] != b'\n' && self.pending[i] != b'\r' {
                i += 1;
                continue;
            }
            let line = std::str::from_utf8(&self.pending[start..i])
                .map_err(|_| "SSE stream contains invalid UTF-8".to_string())?;
            if line.is_empty() {
                if self.has_data {
                    if self.data.ends_with('\n') {
                        self.data.pop();
                    }
                    frames.push(Frame {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data),
                    });
                    self.has_data = false;
                } else {
                    self.event.clear();
                }
            } else if !line.starts_with(':') {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => self.event = value.to_owned(),
                    "data" => {
                        self.data.push_str(value);
                        self.data.push('\n');
                        self.has_data = true;
                    }
                    _ => {}
                }
            }
            if self.pending[i] == b'\r' {
                if self.pending.get(i + 1) == Some(&b'\n') {
                    i += 1;
                } else if i + 1 == self.pending.len() {
                    self.skip_lf = true;
                }
            }
            i += 1;
            start = i;
        }
        self.pending.drain(..start);
        Ok(frames)
    }
}
fn decode(frame: Frame) -> Result<Option<WireEvent>, String> {
    let value: serde_json::Value =
        serde_json::from_str(&frame.data).map_err(|e| format!("invalid SSE JSON: {e}"))?;
    let kind = value
        .get("type")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "SSE JSON has no type".to_string())?
        .to_owned();
    let known = matches!(
        kind.as_str(),
        "hello"
            | "project_updated"
            | "project_removed"
            | "instance_updated"
            | "instance_removed"
            | "listeners_changed"
            | "suggestions_changed"
            | "service_updated"
            | "system_health"
            | "operation_progress"
            | "log"
            | "snapshot_complete"
            | "resync"
    );
    if !known {
        return Ok(None);
    }
    if !frame.event.is_empty() && frame.event != "message" && frame.event != kind {
        return Err(format!(
            "SSE event name {:?} does not match type {kind:?}",
            frame.event
        ));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| format!("invalid {kind} SSE payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_every_byte_boundary_and_multiline_data() {
        let input = ": ping\r\nevent: log\r\ndata: {\"type\":\"log\",\r\ndata: \"instance\":\"myapp\",\"stream\":\"stdout\",\"line\":\"café\",\"ts\":\"2026-11-06T17:52:03.412Z\"}\r\n\r\n";
        let mut parser = Parser::default();
        let mut frames = Vec::new();
        for byte in input.bytes() {
            frames.extend(parser.feed(&[byte]).unwrap());
        }
        // A CRLF pair's last LF is present, so the last blank line is fully dispatched.
        assert_eq!(frames.len(), 1);
        match decode(frames.remove(0)).unwrap().unwrap() {
            WireEvent::Log { line } => assert_eq!(line.line, "café"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn carriage_return_at_end_of_stream_dispatches_the_frame() {
        let mut parser = Parser::default();
        let bytes = b"event: snapshot_complete\rdata: {\"type\":\"snapshot_complete\"}\r\r";
        let frames = parser.feed(bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            decode(frames.into_iter().next().unwrap()).unwrap(),
            Some(WireEvent::SnapshotComplete)
        ));
    }

    #[test]
    fn mismatched_event_name_is_rejected_and_unknown_kind_is_skipped() {
        assert!(
            decode(Frame {
                event: "log".into(),
                data: "{\"type\":\"snapshot_complete\"}".into()
            })
            .is_err()
        );
        assert!(
            decode(Frame {
                event: "future".into(),
                data: "{\"type\":\"future\"}".into()
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn backoff_stays_within_the_documented_jitter() {
        for (attempt, base) in [0.5, 1.0, 2.0, 4.0, 8.0, 15.0, 15.0].into_iter().enumerate() {
            for _ in 0..10 {
                let seconds = backoff(attempt as u32).as_secs_f64();
                assert!(seconds >= base * 0.8 && seconds <= base * 1.2);
            }
        }
    }
}
