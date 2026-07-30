//! The Server-Sent Events endpoint.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use futures::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

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

    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|received| async move {
        match received {
            Ok(event) => Some(Ok(Event::default()
                .event(event.name())
                .data(event.data().to_string()))),
            // A client that fell behind should re-fetch state; events are not
            // replayed, which is why Last-Event-ID is not supported.
            Err(error) => {
                tracing::warn!(%error, "SSE client fell behind");
                None
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keepalive"))
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
}
