//! Snapshot of the generated OpenAPI document.
//!
//! Every endpoint or DTO change shows up as a reviewable diff here, which is
//! what stops the published API reference drifting from the code.
//!
//! Refresh with `UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot`.

use std::path::PathBuf;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/openapi.snapshot.json")
}

#[test]
fn openapi_document_matches_the_snapshot() {
    let generated = suede::api::docs::openapi_document();
    let path = snapshot_path();

    if std::env::var("UPDATE_SNAPSHOT").is_ok() {
        std::fs::write(&path, format!("{generated}\n")).expect("failed to write snapshot");
        eprintln!("updated {}", path.display());
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}\n\
             Run `UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot` to create it.",
            path.display()
        )
    });

    // Compare parsed values, so formatting alone never fails the test.
    let generated_json: serde_json::Value =
        serde_json::from_str(&generated).expect("generated document is valid JSON");
    let expected_json: serde_json::Value =
        serde_json::from_str(&expected).expect("snapshot is valid JSON");

    if generated_json != expected_json {
        let generated_paths = path_names(&generated_json);
        let expected_paths = path_names(&expected_json);

        let added: Vec<&String> = generated_paths.difference(&expected_paths).collect();
        let removed: Vec<&String> = expected_paths.difference(&generated_paths).collect();

        panic!(
            "the OpenAPI document changed.\n\
             added paths:   {added:?}\n\
             removed paths: {removed:?}\n\
             Review the change, then refresh with \
             `UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot`."
        );
    }
}

fn path_names(document: &serde_json::Value) -> std::collections::BTreeSet<String> {
    document["paths"]
        .as_object()
        .map(|paths| paths.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn every_documented_path_is_under_the_version_prefix() {
    let document: serde_json::Value =
        serde_json::from_str(&suede::api::docs::openapi_document()).unwrap();
    for path in document["paths"].as_object().unwrap().keys() {
        assert!(
            path.starts_with("/api/v1/"),
            "{path} is not under the versioned prefix"
        );
    }
}
