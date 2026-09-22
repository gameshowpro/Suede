//! The Server-Sent Events endpoint.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast;

use crate::events::ServerEvent;

use super::ApiState;

/// Interval of the keep-alive comment, so intermediaries do not time the
/// connection out.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[utoipa::path(
    get, path = "/api/v1/events", tag = "events",
    responses((status = 200, description = "Stream of named server-sent events",
               content_type = "text/event-stream"))
)]
pub async fn stream(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    tracing::debug!(
        listeners = state.events.listener_count() + 1,
        "SSE client connected"
    );

    // `config_changed` now carries the whole document, so a client that
    // writes at a high rate (a slider dragging the arrangement endpoint, for
    // instance) can publish many per second. Nothing here waits: a lone
    // event still goes straight out. But once an event is in hand, whatever
    // is *already* queued behind it is drained without waiting, and of that
    // drained run only the newest `config_changed` is forwarded — every
    // other event is still forwarded, in order.
    let stream = stream::unfold(state.events.subscribe(), next_burst)
        .flat_map(stream::iter)
        .map(|event| {
            Ok(Event::default()
                .event(event.name())
                .data(event.data().to_string()))
        });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keepalive"))
}

/// Wait for the next event, then coalesce it with a burst of anything
/// already queued behind it. Returns `None` once the hub's sender is
/// dropped, ending the stream.
async fn next_burst(
    mut receiver: broadcast::Receiver<ServerEvent>,
) -> Option<(Vec<ServerEvent>, broadcast::Receiver<ServerEvent>)> {
    loop {
        match receiver.recv().await {
            Ok(event) => {
                let batch = drain_burst(&mut receiver, event);
                return Some((batch, receiver));
            }
            // A client that fell behind should re-fetch state; events are not
            // replayed, which is why Last-Event-ID is not supported.
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "SSE client fell behind");
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// Starting from an already-received `first` event, drain everything else
/// already queued on `receiver` without waiting for more to arrive. Every
/// non-`config_changed` event is kept, in order; a `config_changed` event
/// replaces whatever earlier one is already in the batch, so only the
/// newest of the drained run survives.
fn drain_burst(
    receiver: &mut broadcast::Receiver<ServerEvent>,
    first: ServerEvent,
) -> Vec<ServerEvent> {
    let mut batch = Vec::new();
    let mut config_slot = None;
    push_coalescing(&mut batch, &mut config_slot, first);

    loop {
        match receiver.try_recv() {
            Ok(event) => push_coalescing(&mut batch, &mut config_slot, event),
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => break,
            // The dropped events are stale; keep draining so a later,
            // still-queued event (or the next receive) still carries the
            // newest document.
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "SSE client fell behind while draining a burst");
            }
        }
    }
    batch
}

/// Push `event` onto `batch`, collapsing repeated `config_changed` events
/// into the slot recorded by `config_slot` so only the newest survives.
fn push_coalescing(
    batch: &mut Vec<ServerEvent>,
    config_slot: &mut Option<usize>,
    event: ServerEvent,
) {
    if matches!(event, ServerEvent::ConfigChanged(_)) {
        match *config_slot {
            Some(index) => batch[index] = event,
            None => {
                batch.push(event);
                *config_slot = Some(batch.len() - 1);
            }
        }
    } else {
        batch.push(event);
    }
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::harness;
    use crate::events::ServerEvent;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use futures::StreamExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn stream_has_the_event_stream_content_type() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
    }

    #[tokio::test]
    async fn published_events_reach_the_stream() {
        let harness = harness(None);
        let events = harness.state.events.clone();

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let mut body = response.into_body().into_data_stream();

        // Publish only once the stream is subscribed.
        events.publish(ServerEvent::OutputsChanged(Vec::new()));

        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("stream should yield promptly")
            .expect("stream should not end")
            .unwrap();
        let text = String::from_utf8_lossy(&chunk);

        assert!(text.contains("event: outputs_changed"), "got: {text}");
        assert!(text.contains("data: []"), "got: {text}");
    }

    #[tokio::test]
    async fn stream_requires_a_token_when_one_is_configured() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A burst of `config_changed` events, as a high-rate writer (a slider
    /// driving the arrangement endpoint, for instance) produces, collapses
    /// into a single `config_changed` carrying the newest document, while
    /// every other event queued in the same burst still comes through.
    #[tokio::test]
    async fn a_burst_of_config_changes_coalesces_to_the_newest_one() {
        use crate::state::StateVersion;

        let harness = harness(None);
        let events = harness.state.events.clone();
        let document = harness.state.store.effective();

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let mut body = response.into_body().into_data_stream();

        // Publish only once the stream is subscribed, so every event below
        // is queued behind the first before anything is polled.
        for generation in 1..=5u64 {
            let version = StateVersion {
                revision: 1,
                generation,
                epoch: "epoch".to_string(),
            };
            events.publish(ServerEvent::ConfigChanged(Box::new(
                crate::api::config_change("projection", &document, &version),
            )));
        }
        events.publish(ServerEvent::StatusChanged(Box::default()));

        let mut config_changed_count = 0;
        let mut saw_status_changed = false;
        let mut saw_last_generation = false;

        loop {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                .await
                .expect("stream should yield promptly")
                .expect("stream should not end")
                .unwrap();
            let text = String::from_utf8_lossy(&chunk).to_string();

            config_changed_count += text.matches("event: config_changed").count();
            if text.contains("event: status_changed") {
                saw_status_changed = true;
            }
            if text.contains("event: config_changed") && text.contains("\"generation\":5") {
                saw_last_generation = true;
            }

            if saw_status_changed && config_changed_count > 0 {
                break;
            }
        }

        assert_eq!(
            config_changed_count, 1,
            "the burst of five config_changed events must coalesce to one"
        );
        assert!(
            saw_status_changed,
            "the status_changed event must still arrive"
        );
        assert!(
            saw_last_generation,
            "the surviving config_changed must carry the newest generation"
        );
    }
}
