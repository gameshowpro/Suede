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
        let mut document: serde_json::Value =
            serde_json::from_str(&generated).expect("generated document is valid JSON");
        blank_the_version(&mut document);
        let text = serde_json::to_string_pretty(&document).expect("snapshot is serialisable");
        std::fs::write(&path, format!("{text}\n")).expect("failed to write snapshot");
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
    let mut generated_json: serde_json::Value =
        serde_json::from_str(&generated).expect("generated document is valid JSON");
    let mut expected_json: serde_json::Value =
        serde_json::from_str(&expected).expect("snapshot is valid JSON");

    // The version is `Cargo.toml`'s, not a fact about the API's shape. Pinning
    // it here meant every release failed this test for a reason that was never
    // drift, and the fix - refresh the snapshot - buried the real diff in noise.
    blank_the_version(&mut generated_json);
    blank_the_version(&mut expected_json);

    if generated_json != expected_json {
        let generated_paths = path_names(&generated_json);
        let expected_paths = path_names(&expected_json);

        let added: Vec<&String> = generated_paths.difference(&expected_paths).collect();
        let removed: Vec<&String> = expected_paths.difference(&generated_paths).collect();

        // A change inside an existing path or schema leaves both lists empty,
        // and "added: [], removed: []" tells you nothing at all - which is
        // exactly what CI reported when the version alone had moved.
        let mut detail = format!("added paths:   {added:?}\nremoved paths: {removed:?}");
        if added.is_empty() && removed.is_empty() {
            detail.push_str(&format!(
                "\nthe paths are unchanged, so the difference is inside one of \
                 them or in the schemas.\ndiffering top-level sections: {:?}",
                differing_sections(&generated_json, &expected_json)
            ));
        }

        panic!(
            "the OpenAPI document changed.\n{detail}\n\
             Review the change, then refresh with \
             `UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot`."
        );
    }
}

/// Replaces the version with a placeholder, so the snapshot never restates a
/// number that `Cargo.toml` already owns.
fn blank_the_version(document: &mut serde_json::Value) {
    if let Some(version) = document.get_mut("info").and_then(|i| i.get_mut("version")) {
        *version = serde_json::Value::String("(set from Cargo.toml)".into());
    }
}

/// Which top-level keys differ, to point at where to look.
fn differing_sections(left: &serde_json::Value, right: &serde_json::Value) -> Vec<String> {
    let empty = serde_json::Map::new();
    let left_map = left.as_object().unwrap_or(&empty);
    let right_map = right.as_object().unwrap_or(&empty);
    left_map
        .keys()
        .chain(right_map.keys())
        .filter(|key| left_map.get(*key) != right_map.get(*key))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
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
