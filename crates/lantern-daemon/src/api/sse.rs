//! Snapshot-first, bounded SSE streams with independent log subscriptions.

use super::{ApiState, dispatch::within, error::HttpError};
use axum::response::{
    IntoResponse, Response,
    sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt, stream::SelectAll};
use lantern_core::{
    Timestamp,
    api::{API_VERSION, Event, OperationId, WireEvent},
    ids::InstanceId,
};
use serde::Deserialize;
use std::{io, pin::Pin};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
struct Query {
    #[serde(default = "yes")]
    state: bool,
    logs: Option<String>,
    tail: Option<u32>,
    op: Option<OperationId>,
}
fn yes() -> bool {
    true
}
type LiveStream = Pin<Box<dyn Stream<Item = Result<WireEvent, u64>> + Send>>;
fn bus_stream(mut rx: broadcast::Receiver<Event>, state: bool, op: Option<OperationId>) -> LiveStream {
    Box::pin(async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let include = state || matches!(&event, Event::OperationProgress { op: id, .. } if Some(id) == op.as_ref());
                    if include { yield Ok(event.into()); }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => { yield Err(skipped); break; }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
fn log_stream(mut rx: broadcast::Receiver<lantern_core::model::LogLine>) -> LiveStream {
    Box::pin(async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(line) => yield Ok(WireEvent::Log { line }),
                Err(broadcast::error::RecvError::Lagged(skipped)) => { yield Err(skipped); break; }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
fn frame(event: WireEvent) -> Result<SseEvent, serde_json::Error> {
    let name = event.event_name();
    Ok(SseEvent::default()
        .event(name)
        .data(serde_json::to_string(&event)?))
}
pub(crate) async fn stream(
    state: ApiState,
    query: &str,
    shutdown: CancellationToken,
) -> Result<Response, HttpError> {
    let q: Query = serde_urlencoded::from_str(query)
        .map_err(|e| HttpError::new("bad_request", format!("invalid query: {e}")))?;
    let ids = match &q.logs {
        None => Vec::new(),
        Some(s) if s.is_empty() => return Err(HttpError::new("bad_request", "logs requires instance ids")),
        Some(s) => s
            .split(',')
            .map(|id| InstanceId::parse(id).map_err(|e| HttpError::new("bad_request", e.to_string())))
            .collect::<Result<Vec<_>, _>>()?,
    };
    if ids.len() > 8 {
        return Err(HttpError::new("bad_request", "at most eight log streams"));
    }
    let tail = q.tail.unwrap_or(100);
    if tail > 1000 {
        return Err(HttpError::new("bad_request", "tail must be at most 1000"));
    }
    let permit = state
        .sse_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| HttpError::new("unavailable", "too many SSE streams"))?;
    // Subscribe before obtaining the snapshot; events during the snapshot are safely replayed.
    let bus = state.bus.subscribe();
    let mut live: SelectAll<LiveStream> = SelectAll::new();
    if q.state || q.op.is_some() {
        live.push(bus_stream(bus, q.state, q.op));
    }
    let mut backlogs = Vec::new();
    for id in ids {
        let (backlog, rx) = within(&state, "logs", state.facade.subscribe_logs(id, tail as usize)).await?;
        backlogs.extend(backlog.into_iter().map(|line| WireEvent::Log { line }));
        live.push(log_stream(rx));
    }
    if live.is_empty() {
        live.push(Box::pin(futures_util::stream::pending()));
    }
    let snapshot = if q.state {
        within(&state, "snapshot", state.facade.event_snapshot()).await?
    } else {
        Vec::new()
    };
    let hello = WireEvent::Hello {
        api_version: API_VERSION,
        daemon_version: state.config.version.clone(),
        boot_id: state.config.boot_id.clone(),
        now: Timestamp::now(),
    };
    // Reject an unserializable initial snapshot before committing HTTP headers.
    let initial = std::iter::once(hello)
        .chain(snapshot.into_iter().map(WireEvent::from))
        .chain(backlogs)
        .chain(std::iter::once(WireEvent::SnapshotComplete))
        .map(frame)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| HttpError::new("internal", "could not serialize event snapshot"))?;
    let stream = async_stream::stream! {
        let _permit = permit;
        for frame in initial { yield Ok::<_, io::Error>(frame); }
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                item = live.next() => match item {
                    Some(Ok(event)) => match frame(event) {
                        Ok(frame) => yield Ok(frame),
                        Err(_) => {
                            tracing::warn!("could not serialize live event");
                            yield Err(io::Error::other("could not serialize live event"));
                            break;
                        }
                    },
                    Some(Err(skipped)) => {
                        match frame(WireEvent::Resync { reason: "lagged".into(), skipped: Some(skipped) }) {
                            Ok(frame) => yield Ok(frame),
                            Err(_) => yield Err(io::Error::other("could not serialize resync")),
                        }
                        break;
                    },
                    None => break,
                }
            }
        }
    };
    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(state.config.sse_ping_interval)
                .text("ping"),
        )
        .into_response())
}
