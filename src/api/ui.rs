//! The bundled reference web UI.
//!
//! A single self-contained page, embedded in the binary at compile time. It
//! uses nothing but the public API, so it doubles as a worked example of every
//! endpoint. It is deliberately build-step free: a reference client should be
//! readable, and CI should not need an npm toolchain to ship it.
//!
//! Disabled when a bearer token is configured — a page that had to embed the
//! token would defeat it.

use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;

use super::ApiState;

const INDEX: &str = include_str!("ui/index.html");

pub fn router() -> Router<ApiState> {
    Router::new().route("/", get(index))
}

async fn index() -> impl IntoResponse {
    Html(INDEX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::harness;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn fetch_root(token: Option<&str>) -> (StatusCode, String) {
        let harness = harness(token);
        let mut request = Request::builder().uri("/").body(Body::empty()).unwrap();
        if let Some(token) = token {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        let response = harness.router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn the_ui_is_served_at_the_root() {
        let (status, body) = fetch_root(None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Suede</title>"));
    }

    #[tokio::test]
    async fn the_ui_is_absent_in_token_mode() {
        let (status, _) = fetch_root(Some("secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_page_is_self_contained() {
        // No external origins: an appliance may have no internet access.
        for marker in ["http://", "https://"] {
            for line in INDEX.lines() {
                let is_reference_link =
                    line.contains("check.docsUrl") || line.contains("placeholder");
                if line.contains(&format!("src=\"{marker}")) && !is_reference_link {
                    panic!("external script reference: {line}");
                }
                if line.contains(&format!("href=\"{marker}")) && !is_reference_link {
                    panic!("external stylesheet reference: {line}");
                }
            }
        }
    }

    #[test]
    fn the_page_exercises_every_endpoint_family() {
        for path in [
            "/outputs",
            "/audio/outputs",
            "/apps",
            "/config",
            "/status",
            "/system",
            "/system/checks",
            "/events",
            "/reconcile",
            "/wallpapers",
            "/config/projection",
        ] {
            assert!(INDEX.contains(path), "the UI never calls {path}");
        }
    }

    #[test]
    fn the_page_covers_every_configurable_field() {
        // The UI is the reference client; a field it cannot reach is a field
        // an operator can only set by hand-editing the raw document.
        for field in [
            "activeApp",
            "readiness",
            "background",
            "allowTearing",
            "maxRenderTimeMs",
            "adaptiveSync",
            "heartbeat",
            "env",
            "gamma",
            "testPattern",
        ] {
            assert!(INDEX.contains(field), "the UI never sets {field}");
        }
    }

    #[test]
    fn an_uploaded_image_can_be_put_on_a_display() {
        // The first version of this tab could upload wallpapers and nothing
        // else, leaving no way to reach a screen with one.
        assert!(
            INDEX.contains("/config/backgrounds"),
            "backgrounds cannot be defined"
        );
        assert!(
            INDEX.contains("renderOutputBackgroundChoices"),
            "the Displays tab offers no way to choose one"
        );
        assert!(
            INDEX.contains("display-buttons"),
            "the output editor is reachable only by clicking the diagram"
        );
    }

    #[test]
    fn warnings_offer_a_way_forward() {
        // Every alert should end in either a fix or a documentation link, and
        // a fix writes to the operator's machine, so it must be confirmed
        // rather than applied on one stray click.
        assert!(INDEX.contains("fixAvailable"), "fixable checks unused");
        assert!(
            INDEX.contains("fixDescription"),
            "the fix is applied unexplained"
        );
        assert!(INDEX.contains("/fix"), "the fix endpoint is never called");
        assert!(
            INDEX.contains("docsUrl"),
            "documentation links are never shown"
        );
        assert!(
            INDEX.contains("divergence.docsUrl"),
            "divergences carry docs links too, and should use them"
        );
    }

    #[test]
    fn the_page_subscribes_to_every_event() {
        for event in [
            "outputs_changed",
            "audio_outputs_changed",
            "app_status_changed",
            "status_changed",
            "config_changed",
            "checks_changed",
        ] {
            assert!(INDEX.contains(event), "the UI ignores {event}");
        }
    }

    #[test]
    fn user_supplied_values_are_escaped() {
        // Output names, app ids, and check details all come from outside.
        assert!(INDEX.contains("const escape ="));
        assert!(INDEX.contains("&amp;"));
    }
}
