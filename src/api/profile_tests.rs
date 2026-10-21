//! Behavioral tests for the System Profiles endpoints.
//!
//! Registered from `api::mod` because the API test harness is intentionally
//! available to the library's unit tests, not to integration-test crates.

use super::test_support::{harness, Harness};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(
    harness: &Harness,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request_body = body
        .map(|value| serde_json::to_vec(&value).expect("request JSON serializes"))
        .unwrap_or_default();
    let response = harness
        .router
        .clone()
        .oneshot(request.body(Body::from(request_body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

async fn current_config(harness: &Harness) -> (HeaderMap, Value) {
    let (status, headers, body) = call(harness, "GET", "/api/v1/config", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (headers, body)
}

fn condition_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    ["etag", "x-config-generation", "x-config-epoch"]
        .into_iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    let name = match name {
                        "etag" => "if-match",
                        "x-config-generation" => "if-config-generation",
                        _ => "if-config-epoch",
                    };
                    (name.to_string(), value.to_string())
                })
        })
        .collect()
}

fn borrowed_headers(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

fn app(id: &str) -> Value {
    json!({"id": id, "launcher": {"kind": "chromium-kiosk", "uri": "https://example.test/"}})
}

#[tokio::test]
async fn create_captures_effective_config_and_export_includes_assets_field() {
    let harness = harness(None);
    let (_, mut config) = current_config(&harness).await;
    config["settings"]["hideCursor"] = json!(false);
    config["apps"] = json!([app("stage")]);
    config["activeApp"] = json!("stage");
    config["committed"] = json!(true);
    let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(config), &[]).await;
    assert_eq!(status, StatusCode::OK);

    let create = json!({"id":"show-a", "name":"Opening show"});
    let (status, _, summary) = call(&harness, "POST", "/api/v1/profiles", Some(create), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{summary}");
    assert_eq!(summary["description"], "");
    assert_eq!(summary["includes"]["apps"], true);

    let (status, _, profile) = call(&harness, "GET", "/api/v1/profiles/show-a", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{profile}");
    assert_eq!(profile["formatVersion"], 1);
    assert_eq!(profile["config"]["settings"]["hideCursor"], false);
    assert_eq!(profile["config"]["apps"][0]["id"], "stage");
    assert_eq!(profile["config"]["activeApp"], "stage");
    assert_eq!(profile["assets"].as_object().unwrap().len(), 0);

    let (status, _, listed) = call(&harness, "GET", "/api/v1/profiles", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed[0]["id"], "show-a");
}

#[tokio::test]
async fn import_stores_only_and_activation_preserves_excluded_sections() {
    let harness = harness(None);
    let (_, mut existing) = current_config(&harness).await;
    existing["apps"] = json!([app("keep-me")]);
    existing["activeApp"] = json!("keep-me");
    existing["settings"]["hideCursor"] = json!(true);
    existing["committed"] = json!(true);
    let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(existing), &[]).await;
    assert_eq!(status, StatusCode::OK);

    let (_, mut exported_config) = current_config(&harness).await;
    exported_config["apps"] = json!([]);
    exported_config["activeApp"] = Value::Null;
    exported_config["settings"]["hideCursor"] = json!(false);
    let profile = json!({
        "formatVersion": 1,
        "name": "Screen-only",
        "includes": {"displays": true, "backgrounds": false, "apps": false, "activeApp": false, "settings": true},
        "config": exported_config,
        "assets": {}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"screen-only", "profile":profile})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Import is a storage operation. The live document still has the values
    // that existed before the import.
    let (_, _, config) = call(&harness, "GET", "/api/v1/config", None, &[]).await;
    assert_eq!(config["apps"][0]["id"], "keep-me");
    assert_eq!(config["settings"]["hideCursor"], true);

    let (version_headers, _) = current_config(&harness).await;
    let conditions = condition_headers(&version_headers);
    let headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/screen-only/activate",
        Some(json!({"outputMappings":{}, "audioMappings":{}})),
        &headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, config) = call(&harness, "GET", "/api/v1/config", None, &[]).await;
    assert_eq!(config["apps"][0]["id"], "keep-me");
    assert_eq!(config["activeApp"], "keep-me");
    assert_eq!(config["settings"]["hideCursor"], false);
}

#[tokio::test]
async fn included_empty_apps_replaces_current_catalog_and_missing_active_app_is_invalid() {
    let harness = harness(None);
    let (_, mut config) = current_config(&harness).await;
    config["apps"] = json!([app("current")]);
    config["activeApp"] = Value::Null;
    config["committed"] = json!(true);
    let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(config.clone()), &[]).await;
    assert_eq!(status, StatusCode::OK);

    let profile = json!({
        "formatVersion": 1,
        "name": "Empty catalog",
        "includes": {"displays": false, "backgrounds": false, "apps": true, "activeApp": false, "settings": false},
        "config": {"schemaVersion": config["schemaVersion"], "apps": [], "outputs": [], "backgrounds": [], "settings": config["settings"]},
        "assets": {}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"empty-apps", "profile":profile})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (version_headers, _) = current_config(&harness).await;
    let conditions = condition_headers(&version_headers);
    let headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/empty-apps/activate",
        Some(json!({})),
        &headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, after) = call(&harness, "GET", "/api/v1/config", None, &[]).await;
    assert_eq!(after["apps"], json!([]));
    assert_eq!(after["activeApp"], Value::Null);

    let invalid_profile = json!({
        "formatVersion": 1,
        "name": "Missing app",
        "includes": {"displays": false, "backgrounds": false, "apps": false, "activeApp": true, "settings": false},
        "config": {"schemaVersion": config["schemaVersion"], "apps": [], "outputs": [], "backgrounds": [], "activeApp": "missing", "settings": config["settings"]},
        "assets": {}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"missing-app", "profile":invalid_profile})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (version_headers, _) = current_config(&harness).await;
    let conditions = condition_headers(&version_headers);
    let headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/missing-app/activate",
        Some(json!({})),
        &headers,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn activation_rejects_a_dirty_working_copy() {
    let harness = harness(None);
    let (_, config) = current_config(&harness).await;
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles",
        Some(json!({"id":"saved", "name":"Saved"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut preview = config;
    preview["committed"] = json!(false);
    preview["settings"]["hideCursor"] = json!(false);
    let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(preview), &[]).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/saved/activate",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn activation_checks_config_version_preconditions() {
    let harness = harness(None);
    let (stale_headers, mut config) = current_config(&harness).await;
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles",
        Some(json!({"id":"conditional", "name":"Conditional"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    config["settings"]["hideCursor"] = json!(false);
    config["committed"] = json!(true);
    let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(config), &[]).await;
    assert_eq!(status, StatusCode::OK);

    let conditions = condition_headers(&stale_headers);
    let headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/conditional/activate",
        Some(json!({})),
        &headers,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

const PNG_A: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn profile_config(schema_version: Value) -> Value {
    json!({
        "schemaVersion": schema_version,
        "outputs": [],
        "apps": [],
        "backgrounds": [],
        "settings": {"hideCursor": true, "outputPollIntervalSeconds": 5, "measureCapabilitiesOnStart": true}
    })
}

#[tokio::test]
async fn wallpaper_assets_round_trip_through_export_import_and_activation() {
    let source = harness(None);
    source.state.wallpapers.store("wall", PNG_A).unwrap();
    let (_, mut config) = current_config(&source).await;
    config["backgrounds"] = json!([{"id":"lobby", "wallpaper":"wall", "mode":"fill"}]);
    config["committed"] = json!(true);
    let (status, _, _) = call(&source, "PUT", "/api/v1/config", Some(config), &[]).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = call(
        &source,
        "POST",
        "/api/v1/profiles",
        Some(json!({"id":"wallpaper", "name":"Wallpaper"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, exported) = call(&source, "GET", "/api/v1/profiles/wallpaper", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(exported["assets"]["wall"], json!(PNG_A));

    let destination = harness(None);
    let (status, _, _) = call(
        &destination,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"portable-wallpaper", "profile":exported})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!destination.state.wallpapers.directory().exists());
    let (headers, _) = current_config(&destination).await;
    let conditions = condition_headers(&headers);
    let request_headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &destination,
        "POST",
        "/api/v1/profiles/portable-wallpaper/activate",
        Some(json!({})),
        &request_headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(destination.state.wallpapers.read("wall").unwrap().0, PNG_A);
}

#[tokio::test]
async fn conflicting_imported_wallpaper_does_not_change_live_config_or_existing_asset() {
    let harness = harness(None);
    let existing_asset = [PNG_A, &[1u8][..]].concat();
    harness
        .state
        .wallpapers
        .store("wall", &existing_asset)
        .unwrap();
    let (_, before) = current_config(&harness).await;

    let mut config = profile_config(before["schemaVersion"].clone());
    config["backgrounds"] = json!([{"id":"lobby", "wallpaper":"wall", "mode":"fill"}]);
    let profile = json!({
        "formatVersion": 1,
        "name": "Conflicting image",
        "includes": {"displays": false, "backgrounds": true, "apps": false, "activeApp": false, "settings": false},
        "config": config,
        "assets": {"wall": PNG_A}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"conflict", "profile":profile})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (headers, _) = current_config(&harness).await;
    let conditions = condition_headers(&headers);
    let request_headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/conflict/activate",
        Some(json!({})),
        &request_headers,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, _, after) = call(&harness, "GET", "/api/v1/config", None, &[]).await;
    assert_eq!(
        after, before,
        "failed activation must leave config untouched"
    );
    assert_eq!(
        harness.state.wallpapers.read("wall").unwrap().0,
        existing_asset
    );
}

#[tokio::test]
async fn update_and_delete_only_change_the_profile_catalog() {
    let harness = harness(None);
    let (_, config) = current_config(&harness).await;
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles",
        Some(json!({"id":"lifecycle", "name":"Initial"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut changed = config.clone();
    changed["settings"]["hideCursor"] = json!(false);
    changed["committed"] = json!(true);
    let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(changed), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(
        &harness,
        "PUT",
        "/api/v1/profiles/lifecycle",
        Some(
            json!({"name":"Updated", "description":"Second capture", "includes":{"settings":true}}),
        ),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, profile) = call(&harness, "GET", "/api/v1/profiles/lifecycle", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(profile["name"], "Updated");
    assert_eq!(profile["description"], "Second capture");
    assert_eq!(profile["config"]["settings"]["hideCursor"], false);

    let (_, live_before_delete) = current_config(&harness).await;
    let (status, _, _) = call(&harness, "DELETE", "/api/v1/profiles/lifecycle", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call(&harness, "GET", "/api/v1/profiles/lifecycle", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, live_after_delete) = current_config(&harness).await;
    assert_eq!(live_after_delete, live_before_delete);
}

#[tokio::test]
async fn activation_maps_saved_display_and_audio_names() {
    let harness = harness(None);
    let (_, current) = current_config(&harness).await;
    let mut config = profile_config(current["schemaVersion"].clone());
    config["outputs"] = json!([
        {"match":{"name":"Saved-A"}, "enable":true},
        {"match":{"name":"Saved-B"}, "enable":true}
    ]);
    config["apps"] = json!([
        {"id":"stage-a", "launcher":{"kind":"chromium-kiosk", "uri":"https://example.test/a"}, "audio":{"output":"saved.sink.a", "gainDb":0.0}},
        {"id":"stage-b", "launcher":{"kind":"chromium-kiosk", "uri":"https://example.test/b"}, "audio":{"output":"saved.sink.b", "gainDb":0.0}}
    ]);
    config["activeApp"] = json!("stage-a");
    let profile = json!({
        "formatVersion": 1,
        "name": "Mapped devices",
        "includes": {"displays": true, "backgrounds": false, "apps": true, "activeApp": true, "settings": false},
        "config": config,
        "assets": {}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"mapped", "profile":profile})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (headers, _) = current_config(&harness).await;
    let conditions = condition_headers(&headers);
    let request_headers = borrowed_headers(&conditions);
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/mapped/activate",
        Some(json!({
            "outputMappings":{"Saved-A":"Saved-B", "Saved-B":"Saved-A"},
            "audioMappings":{"saved.sink.a":"saved.sink.b", "saved.sink.b":"saved.sink.a"}
        })),
        &request_headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, applied) = call(&harness, "GET", "/api/v1/config", None, &[]).await;
    assert_eq!(applied["outputs"][0]["match"]["name"], "Saved-B");
    assert_eq!(applied["outputs"][1]["match"]["name"], "Saved-A");
    assert_eq!(applied["apps"][0]["audio"]["output"], "saved.sink.b");
    assert_eq!(applied["apps"][1]["audio"]["output"], "saved.sink.a");
}

#[tokio::test]
async fn import_rejects_future_format_and_activation_ignores_profile_metadata() {
    let harness = harness(None);
    let (_, current) = current_config(&harness).await;
    let mut config = profile_config(current["schemaVersion"].clone());
    config["revision"] = json!(987654);
    config["committed"] = json!(true);
    let future = json!({
        "formatVersion": 2,
        "name": "Future",
        "includes": {"displays": false, "backgrounds": false, "apps": false, "activeApp": false, "settings": false},
        "config": config,
        "assets": {}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"future", "profile":future})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let mut config = profile_config(current["schemaVersion"].clone());
    config["revision"] = json!(987654);
    config["committed"] = json!(true);
    config["settings"]["hideCursor"] = json!(false);
    let profile = json!({
        "formatVersion": 1,
        "name": "Normalized metadata",
        "includes": {"displays": false, "backgrounds": false, "apps": false, "activeApp": false, "settings": true},
        "config": config,
        "assets": {}
    });
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles/import",
        Some(json!({"id":"normalized", "profile":profile})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, stored) = call(&harness, "GET", "/api/v1/profiles/normalized", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored["config"]["revision"], 0);
    assert_eq!(stored["config"]["committed"], false);

    let (headers, before) = current_config(&harness).await;
    let conditions = condition_headers(&headers);
    let request_headers = borrowed_headers(&conditions);
    let (status, _, activated) = call(
        &harness,
        "POST",
        "/api/v1/profiles/normalized/activate",
        Some(json!({})),
        &request_headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        activated["revision"],
        before["revision"].as_u64().unwrap() + 1
    );
    assert_eq!(activated["committed"], true);
    assert_eq!(activated["settings"]["hideCursor"], false);
}

#[tokio::test]
async fn activation_rejects_each_stale_version_header_independently() {
    for header in ["if-match", "if-config-generation", "if-config-epoch"] {
        let harness = harness(None);
        let (fresh_headers, _) = current_config(&harness).await;
        let (status, _, _) = call(
            &harness,
            "POST",
            "/api/v1/profiles",
            Some(json!({"id":"stale", "name":"Stale"})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, current) = call(&harness, "GET", "/api/v1/config", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        let mut config = current;
        config["committed"] = json!(true);
        config["settings"]["hideCursor"] = json!(false);
        let (status, _, _) = call(&harness, "PUT", "/api/v1/config", Some(config), &[]).await;
        assert_eq!(status, StatusCode::OK);

        let stale_value = match header {
            "if-match" => "\"999999\"",
            "if-config-generation" => "999999",
            _ => "stale-epoch",
        };
        let (status, _, _) = call(
            &harness,
            "POST",
            "/api/v1/profiles/stale/activate",
            Some(json!({})),
            &[(header, stale_value)],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{header}");
        assert!(fresh_headers.contains_key("etag"));
    }
}

#[tokio::test]
async fn new_profiles_require_nonempty_unique_names() {
    let harness = harness(None);
    let (_, before) = current_config(&harness).await;
    let (status, _, _) = call(
        &harness,
        "POST",
        "/api/v1/profiles",
        Some(json!({"id":"first", "name":"Demo"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    for (name, expected) in [
        ("  ", StatusCode::UNPROCESSABLE_ENTITY),
        (" demo ", StatusCode::CONFLICT),
    ] {
        let (status, _, _) = call(
            &harness,
            "POST",
            "/api/v1/profiles",
            Some(json!({"id":"second", "name":name})),
            &[],
        )
        .await;
        assert_eq!(status, expected);
        let (_, _, mut profile) = call(&harness, "GET", "/api/v1/profiles/first", None, &[]).await;
        profile["name"] = json!(name);
        let (status, _, _) = call(
            &harness,
            "POST",
            "/api/v1/profiles/import",
            Some(json!({"id":"imported", "profile":profile})),
            &[],
        )
        .await;
        assert_eq!(status, expected);
    }
    let (status, _, _) = call(
        &harness,
        "PUT",
        "/api/v1/profiles/first",
        Some(json!({"name":"Demo"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, profiles) = call(&harness, "GET", "/api/v1/profiles", None, &[]).await;
    assert_eq!(profiles.as_array().unwrap().len(), 1);
    assert_eq!(current_config(&harness).await.1, before);
}
