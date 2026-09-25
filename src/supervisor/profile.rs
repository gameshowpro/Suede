//! Seeding a Chromium profile with a persistent camera/microphone grant.
//!
//! `--auto-accept-camera-and-microphone-capture` (see
//! [`super::launcher::CHROMIUM_KIOSK_ARGS`]) lets every `getUserMedia` call
//! through, but never persists a grant, so `enumerateDevices()` returns empty
//! `label`/`deviceId` and a page cannot pick a device by name. Writing a site
//! permission straight into the profile's `Default/Preferences` before launch
//! is what actually persists one: Chrome then reports the origin's
//! `navigator.permissions` as `granted` and `enumerateDevices()` returns real
//! labels and ids before any `getUserMedia`, which is what `deviceId: {exact}`
//! selection needs.

use std::io;
use std::path::Path;

use serde_json::{Map, Value};

/// Write, or remove, a persistent camera/microphone grant for `origin` in the
/// Chromium profile at `profile_dir`.
///
/// The entry lives at `profile_dir/Default/Preferences`, under
/// `profile.content_settings.exceptions.media_stream_camera` and
/// `...media_stream_mic`, keyed by `"{origin},*"` — the minimal form Chrome
/// itself accepts and writes.
///
/// `grant == true` sets both entries to `{"setting": 1}` ("allow"), merging
/// into whatever the file already holds and creating `Default/` and the file
/// itself if neither exists yet. `grant == false` removes only this origin's
/// two entries — leaving every other key, and every other origin's grant,
/// exactly as it was — and does nothing at all if the file is absent, since
/// there is then nothing to revoke.
///
/// If the file exists but does not parse, or holds something other than a
/// JSON object anywhere along the path being written or read, this returns
/// an [`io::ErrorKind::InvalidData`] error and leaves the file untouched: a
/// document this malformed is not one Suede should guess at fixing.
///
/// The write is atomic — a temp file in the same directory, written and
/// synced, then renamed over the target — so a launch racing a crash never
/// finds a half-written `Preferences`, and Chrome rewriting the file itself
/// on exit is never observed mid-write.
pub fn apply_capture_grant(profile_dir: &Path, origin: &str, grant: bool) -> io::Result<()> {
    let path = profile_dir.join("Default").join("Preferences");
    let key = format!("{origin},*");

    if !grant {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut doc = parse_object(&bytes)?;
        let mut changed = false;
        for map_name in EXCEPTION_MAPS {
            if remove_exception(&mut doc, map_name, &key)? {
                changed = true;
            }
        }
        if changed {
            write_atomically(&path, &doc)?;
        }
        return Ok(());
    }

    let mut doc = match std::fs::read(&path) {
        Ok(bytes) => parse_object(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => return Err(error),
    };
    for map_name in EXCEPTION_MAPS {
        set_exception(&mut doc, map_name, &key)?;
    }
    write_atomically(&path, &doc)
}

const EXCEPTION_MAPS: [&str; 2] = ["media_stream_camera", "media_stream_mic"];

fn parse_object(bytes: &[u8]) -> io::Result<Value> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| invalid_data(error.to_string()))?;
    if !value.is_object() {
        return Err(invalid_data("Preferences root is not a JSON object"));
    }
    Ok(value)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Set `doc.profile.content_settings.exceptions[map_name][key]`, creating
/// every missing object along the way.
fn set_exception(doc: &mut Value, map_name: &str, key: &str) -> io::Result<()> {
    let map = exceptions_map(doc, map_name)?;
    map.insert(key.to_string(), serde_json::json!({"setting": 1}));
    Ok(())
}

/// Remove `key` from `doc.profile.content_settings.exceptions[map_name]`,
/// reporting whether it was present. A missing intermediate object is not an
/// error here — there is simply nothing to remove — but one present and not
/// an object still is, the same as for [`set_exception`].
fn remove_exception(doc: &mut Value, map_name: &str, key: &str) -> io::Result<bool> {
    let root = doc.as_object_mut().expect("doc is an object; see parse_object");
    let Some(profile) = optional_child(root, "profile")? else {
        return Ok(false);
    };
    let Some(content_settings) = optional_child(profile, "content_settings")? else {
        return Ok(false);
    };
    let Some(exceptions) = optional_child(content_settings, "exceptions")? else {
        return Ok(false);
    };
    let Some(map) = optional_child(exceptions, map_name)? else {
        return Ok(false);
    };
    Ok(map.remove(key).is_some())
}

/// `doc.profile.content_settings.exceptions[map_name]`, creating every
/// missing object along the way.
fn exceptions_map<'a>(doc: &'a mut Value, map_name: &str) -> io::Result<&'a mut Map<String, Value>> {
    let root = doc.as_object_mut().expect("doc is an object; see parse_object");
    let profile = required_child(root, "profile")?;
    let content_settings = required_child(profile, "content_settings")?;
    let exceptions = required_child(content_settings, "exceptions")?;
    required_child(exceptions, map_name)
}

/// Get `map[key]` as an object, creating it empty if absent. Errors, without
/// modifying `map`, if it holds something else.
fn required_child<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> io::Result<&'a mut Map<String, Value>> {
    match map.entry(key).or_insert_with(|| Value::Object(Map::new())) {
        Value::Object(child) => Ok(child),
        _ => Err(invalid_data(format!(
            "Preferences {key:?} is not a JSON object"
        ))),
    }
}

/// As [`required_child`], but `None` rather than creating anything when the
/// key is simply absent.
fn optional_child<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> io::Result<Option<&'a mut Map<String, Value>>> {
    match map.get_mut(key) {
        None => Ok(None),
        Some(Value::Object(child)) => Ok(Some(child)),
        Some(_) => Err(invalid_data(format!(
            "Preferences {key:?} is not a JSON object"
        ))),
    }
}

/// Temp file in `path`'s own directory, written and synced, then renamed over
/// `path` — so a reader never observes a partial write, and a launch racing a
/// crash finds either the old file or the new one, never a mix.
fn write_atomically(path: &Path, doc: &Value) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("Preferences path has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(doc).map_err(|error| invalid_data(error.to_string()))?;

    let temp_path = parent.join(format!(".preferences.suede-tmp.{}", std::process::id()));
    write_temp_file(&temp_path, &bytes)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

#[cfg(unix)]
fn write_temp_file(temp_path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(temp_path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_temp_file(temp_path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(temp_path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_json(path: &Path) -> Value {
        let bytes = std::fs::read(path).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn a_fresh_profile_gets_both_entries() {
        let dir = tempfile::tempdir().unwrap();
        apply_capture_grant(dir.path(), "http://10.0.0.5:8080", true).unwrap();

        let prefs = read_json(&dir.path().join("Default").join("Preferences"));
        let key = "http://10.0.0.5:8080,*";
        assert_eq!(
            prefs["profile"]["content_settings"]["exceptions"]["media_stream_camera"][key]
                ["setting"],
            1
        );
        assert_eq!(
            prefs["profile"]["content_settings"]["exceptions"]["media_stream_mic"][key]
                ["setting"],
            1
        );
    }

    #[test]
    fn granting_merges_and_keeps_unrelated_prefs_and_other_origins() {
        let dir = tempfile::tempdir().unwrap();
        let default_dir = dir.path().join("Default");
        std::fs::create_dir_all(&default_dir).unwrap();
        let prefs_path = default_dir.join("Preferences");
        std::fs::write(
            &prefs_path,
            serde_json::to_vec(&json!({
                "some_other_setting": "kept",
                "profile": {
                    "name": "Person 1",
                    "content_settings": {
                        "exceptions": {
                            "media_stream_camera": {
                                "http://other.example,*": {"setting": 1}
                            },
                            "cookies": {
                                "http://other.example,*": {"setting": 1}
                            }
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        apply_capture_grant(dir.path(), "http://10.0.0.5:8080", true).unwrap();

        let prefs = read_json(&prefs_path);
        assert_eq!(prefs["some_other_setting"], "kept");
        assert_eq!(prefs["profile"]["name"], "Person 1");
        let camera = &prefs["profile"]["content_settings"]["exceptions"]["media_stream_camera"];
        assert_eq!(camera["http://other.example,*"]["setting"], 1);
        assert_eq!(camera["http://10.0.0.5:8080,*"]["setting"], 1);
        assert_eq!(
            prefs["profile"]["content_settings"]["exceptions"]["cookies"]
                ["http://other.example,*"]["setting"],
            1
        );
    }

    #[test]
    fn revoking_removes_only_this_origin() {
        let dir = tempfile::tempdir().unwrap();
        apply_capture_grant(dir.path(), "http://a.example", true).unwrap();
        apply_capture_grant(dir.path(), "http://b.example", true).unwrap();

        apply_capture_grant(dir.path(), "http://a.example", false).unwrap();

        let prefs = read_json(&dir.path().join("Default").join("Preferences"));
        let camera = &prefs["profile"]["content_settings"]["exceptions"]["media_stream_camera"];
        assert!(camera.get("http://a.example,*").is_none());
        assert_eq!(camera["http://b.example,*"]["setting"], 1);
    }

    #[test]
    fn revoking_on_a_missing_file_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        apply_capture_grant(dir.path(), "http://a.example", false).unwrap();
        assert!(!dir.path().join("Default").exists());
    }

    #[test]
    fn an_unparseable_file_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let default_dir = dir.path().join("Default");
        std::fs::create_dir_all(&default_dir).unwrap();
        let prefs_path = default_dir.join("Preferences");
        std::fs::write(&prefs_path, b"not json").unwrap();

        let error = apply_capture_grant(dir.path(), "http://a.example", true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&prefs_path).unwrap(), b"not json");
    }

    #[test]
    fn a_non_object_exceptions_map_is_an_error_and_the_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let default_dir = dir.path().join("Default");
        std::fs::create_dir_all(&default_dir).unwrap();
        let prefs_path = default_dir.join("Preferences");
        let original = json!({
            "profile": {"content_settings": {"exceptions": {"media_stream_camera": "not an object"}}}
        });
        std::fs::write(&prefs_path, serde_json::to_vec(&original).unwrap()).unwrap();

        let error = apply_capture_grant(dir.path(), "http://a.example", true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_json(&prefs_path), original);
    }
}
