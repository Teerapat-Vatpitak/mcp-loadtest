//! Background reader task for the SSE transport.
//!
//! Owns the spawned task that parses SSE events off the wire and forwards
//! `message` payloads onto the mpsc channel feeding `SseTransport::request`.
//! Pure mechanics — no public API surface here; everything is `pub(super)`.

use std::fmt::Display;

use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::TransportError;

/// Minimal shape we peel off an incoming frame to extract the JSON-RPC `id`.
/// Notifications (no `id`) are tolerated and surface as `None`.
#[derive(Deserialize)]
pub(super) struct IdProbe {
    pub(super) id: Option<serde_json::Value>,
}

/// Peel the JSON-RPC `id` out of a frame. Returns `None` for notifications or
/// frames without a numeric/string id. The id is returned in its raw JSON
/// form so we can compare against the outbound one with no coercion games.
pub(super) fn extract_id(frame: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<IdProbe>(frame)
        .ok()
        .and_then(|p| p.id)
}

/// Spawn the background reader task.
///
/// `event_stream` is the already-handshaked SSE event source (the initial
/// `endpoint` event has been consumed before this is called). The task
/// forwards `message` payloads onto `tx` and shuts down on either
/// `cancel` firing or the stream ending.
pub(super) fn spawn_reader<S, E>(
    mut event_stream: S,
    tx: mpsc::Sender<Result<String, TransportError>>,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    S: Stream<Item = Result<eventsource_stream::Event, E>> + Send + Unpin + 'static,
    E: Display + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                next = event_stream.next() => match next {
                    Some(Ok(ev)) => {
                        // Only `message` events carry JSON-RPC payloads;
                        // ping / comment / re-emitted endpoint events are
                        // ignored on purpose so they don't confuse the
                        // `request` id-correlation buffer.
                        if ev.event == "message"
                            && tx.send(Ok(ev.data)).await.is_err()
                        {
                            // Consumer dropped — nothing more to do.
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = tx
                            .send(Err(TransportError::Other(format!(
                                "sse parse error: {e}"
                            ))))
                            .await;
                        break;
                    }
                    None => {
                        let _ = tx.send(Err(TransportError::Closed)).await;
                        break;
                    }
                }
            }
        }
    })
}
