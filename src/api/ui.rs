//! The bundled reference web UI.
//!
//! A single self-contained page, embedded in the binary at compile time. It
//! uses nothing but the public API, so it doubles as a worked example of every
//! endpoint. The page needs no frontend toolchain; Cargo generates its artwork
//! from the canonical sources and embeds every asset, including local fonts,
//! in the same binary.
//!
//! Served in token mode too. The page embeds no credential: its API calls
//! succeed only when something in front of Suede, an authenticating proxy,
//! adds the token. Without one the first call returns 401 and the page shows
//! a full-page explanation.

use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;

use super::ApiState;

const INDEX: &str = include_str!("ui/index.html");
include!(concat!(env!("OUT_DIR"), "/artwork.rs"));

// Canonical WOFF2 files are already web-ready; embed them without conversion.
// Keep their licenses available alongside the fonts in distributed binaries.
const FONTS: &[(&str, &str, &[u8])] = &[
    (
        "fonts.css",
        "text/css; charset=utf-8",
        include_bytes!("../../fonts/fonts.css"),
    ),
    (
        "PlusJakartaSans-Variable.woff2",
        "font/woff2",
        include_bytes!("../../fonts/PlusJakartaSans-Variable.woff2"),
    ),
    (
        "PlusJakartaSans-Italic-Variable.woff2",
        "font/woff2",
        include_bytes!("../../fonts/PlusJakartaSans-Italic-Variable.woff2"),
    ),
    (
        "CascadiaCode.woff2",
        "font/woff2",
        include_bytes!("../../fonts/CascadiaCode.woff2"),
    ),
    (
        "CascadiaCodeItalic.woff2",
        "font/woff2",
        include_bytes!("../../fonts/CascadiaCodeItalic.woff2"),
    ),
    (
        "PlusJakartaSans-LICENSE.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../../fonts/PlusJakartaSans-LICENSE.txt"),
    ),
    (
        "CascadiaCode-LICENSE.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../../fonts/CascadiaCode-LICENSE.txt"),
    ),
];

// The license texts the About box links to, served from the appliance so they
// work offline and always match the installed version.
const LEGAL: &[(&str, &str, &[u8])] = &[
    (
        "LICENSE.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../../LICENSE"),
    ),
    (
        "OSMFEULA.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../../OSMFEULA.txt"),
    ),
];

pub fn router() -> Router<ApiState> {
    let mut router = Router::new().route("/", get(index));
    // Register only embedded assets, never paths into the host filesystem.
    for (directory, &(name, mime, bytes)) in ARTWORK
        .iter()
        .map(|asset| ("artwork", asset))
        .chain(FONTS.iter().map(|asset| ("fonts", asset)))
        .chain(LEGAL.iter().map(|asset| ("legal", asset)))
    {
        let serve = move || async move {
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                bytes,
            )
        };
        router = router.route(&format!("/assets/{directory}/{name}"), get(serve));
        if directory == "artwork" && name == "favicon.ico" {
            router = router.route("/favicon.ico", get(serve));
        }
    }
    router
}

async fn index(State(state): State<ApiState>) -> impl IntoResponse {
    Html(render_index(|path| state.bootstrap.docs_url(path)))
}

/// Placeholder in the page for the one-line description, which is written once,
/// in `Cargo.toml`.
const TAGLINE_TOKEN: &str = "{{tagline}}";

/// The page with its documentation links and tagline filled in.
fn render_index(docs_url: impl Fn(&str) -> String) -> String {
    let mut tagline = String::new();
    push_html_attribute(&mut tagline, env!("CARGO_PKG_DESCRIPTION"));
    render_docs_links(INDEX, docs_url).replace(TAGLINE_TOKEN, &tagline)
}

/// Replace static documentation path markers with links using the configured
/// documentation base URL. Links do not load external resources automatically.
fn render_docs_links(html: &str, docs_url: impl Fn(&str) -> String) -> String {
    const MARKER: &str = "data-docs-path=\"";
    let mut rendered = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(MARKER) {
        let value_start = start + MARKER.len();
        let Some(end_offset) = rest[value_start..].find('"') else {
            rendered.push_str(rest);
            return rendered;
        };
        let value_end = value_start + end_offset;
        rendered.push_str(&rest[..start]);
        let path = rest[value_start..value_end].trim_start_matches('/');
        let url = docs_url(path);
        rendered.push_str("href=\"");
        push_html_attribute(&mut rendered, &url);
        rendered.push('"');
        rest = &rest[value_end + 1..];
    }
    rendered.push_str(rest);
    rendered
}

fn push_html_attribute(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '"' => output.push_str("&quot;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ => output.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::harness;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn documentation_paths_use_the_configured_base_and_are_attribute_escaped() {
        let rendered = render_docs_links(
            r#"<a data-docs-path="configuration/#projection-geometry">docs</a>"#,
            |path| format!("https://docs.example.test/root/?q=\"<x>&/{path}"),
        );
        assert_eq!(
            rendered,
            r#"<a href="https://docs.example.test/root/?q=&quot;&lt;x&gt;&amp;/configuration/#projection-geometry">docs</a>"#
        );
    }

    #[test]
    fn rendered_documentation_links_resolve_from_the_root() {
        let rendered = render_docs_links(
            r#"<a data-docs-path="configuration/#projection-geometry">docs</a>"#,
            |path| format!("https://docs.example.test/{path}"),
        );
        assert!(rendered
            .contains(r#"href="https://docs.example.test/configuration/#projection-geometry""#));
        assert!(!rendered.contains("data-docs-path"));
    }

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
        let expected = format!(
            r#"href="{}configuration/#projection-geometry""#,
            crate::config::DEFAULT_DOCS_BASE_URL
        );
        assert!(body.contains(&expected));
        assert!(!body.contains("data-docs-path"));
        // The tagline comes from Cargo.toml, in both places, with no token left.
        let tagline = env!("CARGO_PKG_DESCRIPTION");
        assert!(body.contains(&format!(
            r#"<meta property="og:description" content="{tagline}">"#
        )));
        assert!(body.contains(&format!(r#"<p class="hint">{tagline}</p>"#)));
        assert!(!body.contains(TAGLINE_TOKEN));
    }

    #[tokio::test]
    async fn the_ui_is_served_in_token_mode_and_embeds_no_token() {
        // Without the token header: a browser that reached the appliance
        // directly must get the page that explains the 401, not a JSON error.
        let harness = harness(Some("secret-token-value"));
        let response = harness
            .router
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("<title>Suede</title>"));
        assert!(!body.contains("secret-token-value"));
        assert!(body.contains("id=\"auth-required\""));
        assert!(!body.contains("data-docs-path"));
    }

    #[tokio::test]
    async fn the_ui_does_not_open_the_api_in_token_mode() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/outputs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn the_page_explains_a_401_and_points_at_the_proxy_docs() {
        assert!(INDEX.contains("This appliance requires an API token"));
        assert!(INDEX.contains(r#"data-docs-path="reverse-proxy/#behind-a-reverse-proxy""#));
        assert!(INDEX.contains("response.status === 401"));
    }

    #[tokio::test]
    async fn artwork_fonts_and_legal_texts_are_embedded_and_served_with_their_media_types() {
        let harness = harness(None);
        for (directory, &(name, mime, expected)) in ARTWORK
            .iter()
            .map(|asset| ("artwork", asset))
            .chain(FONTS.iter().map(|asset| ("fonts", asset)))
            .chain(LEGAL.iter().map(|asset| ("legal", asset)))
        {
            let response = harness
                .router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/assets/{directory}/{name}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{name}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime, "{name}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), expected, "{name}");
        }
    }

    #[tokio::test]
    async fn the_license_texts_are_served_as_plain_text() {
        for (name, needle) in [
            ("LICENSE.txt", "Mozilla Public License"),
            ("OSMFEULA.txt", "Open Source Maintenance Fee"),
        ] {
            let response = harness(None)
                .router
                .oneshot(
                    Request::builder()
                        .uri(format!("/assets/legal/{name}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{name}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8",
                "{name}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&bytes).contains(needle),
                "{name} never says {needle:?}"
            );
        }
    }

    #[tokio::test]
    async fn assets_are_served_in_token_mode_and_unknown_assets_are_not() {
        // Static, credential-free files: served without a token so the page
        // renders (and can explain the 401) before any proxy is in play.
        for (token, path, expected) in [
            (
                Some("secret"),
                "/assets/artwork/icon-header.png",
                StatusCode::OK,
            ),
            (
                Some("secret"),
                "/assets/artwork/icon-header.avif",
                StatusCode::OK,
            ),
            (Some("secret"), "/favicon.ico", StatusCode::OK),
            (Some("secret"), "/assets/fonts/fonts.css", StatusCode::OK),
            (
                Some("secret"),
                "/assets/fonts/CascadiaCode.woff2",
                StatusCode::OK,
            ),
            (Some("secret"), "/assets/legal/LICENSE.txt", StatusCode::OK),
            // Unknown paths under /assets/ get past the token check and then
            // 404 like anywhere else; they never reach the host filesystem.
            (
                Some("secret"),
                "/assets/legal/not-a-license.txt",
                StatusCode::NOT_FOUND,
            ),
            (
                Some("secret"),
                "/assets/../Cargo.toml",
                StatusCode::NOT_FOUND,
            ),
            (
                None,
                "/assets/artwork/not-an-icon.png",
                StatusCode::NOT_FOUND,
            ),
            (
                None,
                "/assets/fonts/not-a-font.woff2",
                StatusCode::NOT_FOUND,
            ),
            (
                None,
                "/assets/legal/not-a-license.txt",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let harness = harness(token);
            let response = harness
                .router
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
    }

    /// Every `"..."`-quoted value that follows `prefix` in `source`.
    fn quoted_after<'a>(source: &'a str, prefix: &str) -> Vec<&'a str> {
        source
            .split(prefix)
            .skip(1)
            .filter_map(|rest| rest.split_once('"').map(|(value, _)| value))
            .collect()
    }

    /// The ids passed whole to a lookup call such as `$("name")`. A literal
    /// followed by anything but `)` is the start of a computed id
    /// (`$("power-" + verb)`), which no static check can resolve.
    fn looked_up<'a>(source: &'a str, call: &str) -> Vec<&'a str> {
        source
            .split(call)
            .skip(1)
            .filter_map(|rest| {
                let (value, after) = rest.split_once('"')?;
                after.starts_with(')').then_some(value)
            })
            .collect()
    }

    #[test]
    fn every_element_the_script_looks_up_exists() {
        // A lookup of an id the page no longer has returns null, and the
        // first property access on it throws, abandoning the rest of that
        // render. Removing the interval span from the telemetry hint did
        // exactly that: the telemetry tables stopped filling in, and nothing
        // failed until an operator opened the page.
        let defined: std::collections::BTreeSet<&str> = quoted_after(INDEX, "id=\"")
            .into_iter()
            .chain(quoted_after(INDEX, ".id = \""))
            .collect();
        let missing: Vec<&str> = looked_up(INDEX, "$(\"")
            .into_iter()
            .chain(looked_up(INDEX, "getElementById(\""))
            .filter(|id| !defined.contains(id))
            .collect();
        assert!(
            missing.is_empty(),
            "the script looks up ids no markup or script defines: {missing:?}"
        );
    }

    #[test]
    fn the_page_is_self_contained() {
        // An appliance may have no internet access, so nothing the page needs
        // to render may come from another origin. Hyperlinks are fine: one
        // to GitHub opens in a new tab and fails harmlessly offline. What
        // must never leave the appliance is a script or stylesheet.
        for marker in ["http://", "https://"] {
            for line in INDEX.lines() {
                if line.contains("<script") && line.contains(&format!("src=\"{marker}")) {
                    panic!("external script reference: {line}");
                }
                if line.contains("<link") && line.contains(&format!("href=\"{marker}")) {
                    panic!("external stylesheet reference: {line}");
                }
            }
        }
    }

    #[test]
    fn the_page_states_its_license_and_fee() {
        for needle in [
            "Mozilla Public License",
            "Open Source Maintenance Fee",
            r#"data-docs-path="maintenance-fee/#who-pays""#,
            "https://github.com/sponsors/gameshowpro",
            r#"id="about-dialog""#,
            r#"id="fee-dialog""#,
            r#"class="osmf-mark""#,
            "suede.feeNoticeAcknowledged",
            "/assets/legal/LICENSE.txt",
            "/assets/legal/OSMFEULA.txt",
            r#"aria-label="GameshowPro""#,
            r#"id="footer-about""#,
        ] {
            assert!(INDEX.contains(needle), "the page never says {needle:?}");
        }
    }

    #[test]
    fn the_fee_notice_is_dismissable_once() {
        // Read before it is shown and written when it is dismissed; without
        // the write it would reappear on every visit.
        assert!(
            INDEX.contains(r#"localStorage.getItem("suede.feeNoticeAcknowledged")"#),
            "the notice never checks whether it was already dismissed"
        );
        assert!(
            INDEX.contains(r#"localStorage.setItem("suede.feeNoticeAcknowledged", "1")"#),
            "dismissing the notice is never remembered"
        );
    }

    #[test]
    fn the_page_exercises_every_endpoint_family() {
        for path in [
            "/outputs",
            "/av",
            "/apps",
            "/apps/capabilities",
            "/apps/capabilities/measure",
            "/config",
            "/status",
            "/system",
            "/system/checks",
            "/events",
            "/reconcile",
            "/wallpapers",
            "/config/revert",
            "/ports",
            "/projection/recommendation",
            "/projection/stats",
        ] {
            assert!(INDEX.contains(path), "the UI never calls {path}");
        }
    }

    #[test]
    fn the_page_covers_every_configurable_field() {
        // The UI is the reference client; a field it cannot reach is a field
        // an operator can only set by hand-editing the raw document.
        //
        // The field list is taken from the API schema rather than written out
        // here, because a hand-maintained list only checks what somebody
        // remembered to add to it. `launcher.program` was added to the model,
        // the docs and the OpenAPI document, and this test went on passing
        // while the UI had no way to set it - which is exactly the failure it
        // is supposed to catch.
        //
        // Anything deliberately not offered goes in the list below, with the
        // reason. A new field appears in neither place and fails the test,
        // which turns forgetting into a decision.
        const NOT_OFFERED: &[(&str, &str)] = &[
            ("schemaVersion", "server-managed"),
            ("revision", "server-managed"),
            ("model", "EDID matching; the UI matches by connector name"),
            ("serial", "EDID matching; the UI matches by connector name"),
            ("hideCursor", "no settings tab yet"),
            ("outputPollIntervalSeconds", "no settings tab yet"),
            ("measureCapabilitiesOnStart", "no settings tab yet"),
            // allowRawSwayCommands is deliberately absent from this list: it
            // carries `#[serde(skip_serializing)]` (see
            // `Settings::allow_raw_sway_commands`), which drops it from the
            // OpenAPI schema entirely, so it never reaches `fields` below.
            (
                "policy",
                "restart policy is not exposed; the default suits an appliance",
            ),
            ("delayMs", "restart policy is not exposed"),
            ("maxDelayMs", "restart policy is not exposed"),
            (
                "expectStatus",
                "readiness offers the URL and timings, not the status code",
            ),
            ("showFpsCounter", "a diagnostic, not a configuration choice"),
            ("persistProfile", "kiosk sessions are stateless by design"),
            (
                "sharedInput",
                "experimental Vulkan transport is configured outside the UI",
            ),
            (
                "adopted",
                "read-only: Suede's own pin of what settled, shown beside each field rather than edited",
            ),
            (
                "arrangeOffset",
                "locked at {0, 0}: a per-output nudge for a client driving the grid \
                 arrangement solve directly, not a manual geometry control",
            ),
            (
                "sliceUnits",
                "not offered yet: the \"Slice from canvas\" panel's Percent/Pixels \
                 radio will write it; until then the page keeps whatever unit the \
                 document holds, since an omitted unit is kept",
            ),
        ];

        let document: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/openapi.snapshot.json"))
                .expect("the committed OpenAPI snapshot must parse");
        let schemas = &document["components"]["schemas"];

        fn collect(node: &serde_json::Value, into: &mut Vec<String>) {
            match node {
                serde_json::Value::Object(map) => {
                    for (key, value) in map {
                        if key == "properties" {
                            if let Some(fields) = value.as_object() {
                                into.extend(fields.keys().cloned());
                            }
                        } else {
                            collect(value, into);
                        }
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|i| collect(i, into)),
                _ => {}
            }
        }

        let mut fields = Vec::new();
        // The desired-state types only: observed state is reported, not set.
        for schema in [
            "DesiredState",
            "OutputConfig",
            "AppConfig",
            "Launcher",
            "ProjectionConfig",
            "CanvasConfig",
            "OutputGeometry",
            "CanvasRect",
            "Settings",
            "AudioConfig",
            "HeartbeatConfig",
            "ReadinessConfig",
            "RestartPolicy",
            "BackgroundPreset",
            "Background",
            "Mode",
            "Position",
            "OutputMatch",
        ] {
            let node = &schemas[schema];
            assert!(!node.is_null(), "{schema} is missing from the snapshot");
            collect(node, &mut fields);
        }

        let missing: Vec<&String> = fields
            .iter()
            .filter(|field| !INDEX.contains(field.as_str()))
            .filter(|field| !NOT_OFFERED.iter().any(|(name, _)| name == field))
            .collect();
        assert!(
            missing.is_empty(),
            "the UI cannot set: {missing:?} - offer them, or list them in              NOT_OFFERED with the reason"
        );
    }

    #[test]
    fn geometry_fields_are_wired_to_real_controls() {
        // `the_page_covers_every_configurable_field` above deliberately
        // checks a raw field name anywhere in the file — necessary because it
        // is generated from the whole schema and most of those names (mode,
        // scale, slice, width...) are common English/CSS words that appear
        // constantly outside any form control, so a whole-file substring
        // match there proves nothing beyond "this word exists somewhere".
        //
        // For the fields this round's geometry/canvas fixes actually touch,
        // check something a decorative comment or an unrelated JS variable
        // cannot satisfy: a `data-config-field="<name>"` attribute on a real
        // input/select in the Displays editor. A field could be removed from
        // its control while staying mentioned elsewhere in the page, and the
        // old substring check would not catch it; this one does.
        for field in [
            "enable",
            "mode",
            "scale",
            "transform",
            "background",
            "slice",
            "corners",
            "center",
            "aspect",
            "renderWidth",
        ] {
            let needle = format!("data-config-field=\"{field}\"");
            assert!(
                INDEX.contains(&needle),
                "no control carries {needle:?} — the {field} field is not demonstrably wired to an editable control"
            );
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
            INDEX.contains("output-table"),
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
            "av_changed",
            "app_status_changed",
            "status_changed",
            "config_changed",
            "checks_changed",
            "projection_stats_changed",
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
