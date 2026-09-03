//! The last capability measurement, remembered across restarts.
//!
//! The measurement opens a window on the appliance's displays, so unlike
//! every other health check it must not simply re-run on a schedule. Instead
//! the last report is kept, together with a key describing everything that
//! could change the answer — and the boot-time check measures again only
//! when the key no longer matches. Most boots compare keys, reuse the
//! stored report, and open nothing.

use std::path::PathBuf;
use std::sync::RwLock;

use crate::model::{AppConfig, CapabilityReport, DesiredState, Launcher};

/// Everything that could change what a measurement would say.
///
/// Compared structurally rather than hashed, so a stale measurement can be
/// *explained*: the stored key says what the world looked like when it was
/// taken, and a mismatch names what moved.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasurementKey {
    /// The launcher configuration, verbatim.
    pub launcher: serde_json::Value,
    /// The app's environment overrides — where acceleration is configured.
    pub env: std::collections::BTreeMap<String, String>,
    /// The resolved browser binary and when it was last modified: an
    /// upgraded browser answers afresh rather than inheriting.
    pub program: String,
    pub program_mtime_secs: u64,
    /// GPU driver fingerprint, where one announces itself.
    pub driver: String,
    /// Kernel release, which carries the V4L2 and DRM side of the story.
    pub kernel: String,
    /// The daemon build, whose preset arguments are part of the launch.
    pub build: String,
}

impl MeasurementKey {
    pub fn new(app: &AppConfig, program: &std::path::Path) -> Self {
        let mtime = std::fs::metadata(program)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            launcher: serde_json::to_value(&app.launcher).unwrap_or(serde_json::Value::Null),
            env: app.env.clone().into_iter().collect(),
            program: program.display().to_string(),
            program_mtime_secs: mtime,
            driver: std::fs::read_to_string("/sys/module/nvidia/version")
                .map(|v| format!("nvidia {}", v.trim()))
                .unwrap_or_default(),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|v| v.trim().to_string())
                .unwrap_or_default(),
            build: crate::BUILD_ID.to_string(),
        }
    }
}

/// One measurement, successful or not: a browser that failed to report is a
/// fact worth keeping too, so the health check can say so instead of
/// pretending nothing was tried.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMeasurement {
    /// Unix seconds.
    pub measured_at: u64,
    /// Which application's configuration was measured.
    pub app_id: String,
    pub key: MeasurementKey,
    #[serde(default)]
    pub report: Option<CapabilityReport>,
    /// Why there is no report, when there is none.
    #[serde(default)]
    pub note: Option<String>,
}

/// Persists the latest measurement in the state directory.
pub struct CapabilityStore {
    path: PathBuf,
    latest: RwLock<Option<StoredMeasurement>>,
}

impl CapabilityStore {
    pub fn new(state_dir: &std::path::Path) -> Self {
        let path = state_dir.join("capability-report.json");
        let latest = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        Self {
            path,
            latest: RwLock::new(latest),
        }
    }

    pub fn latest(&self) -> Option<StoredMeasurement> {
        self.latest.read().unwrap().clone()
    }

    /// Remember a measurement, surviving restarts. A failed write keeps the
    /// in-memory copy — the health check still works, only forgetfully.
    pub fn record(&self, measurement: StoredMeasurement) {
        if let Ok(text) = serde_json::to_string_pretty(&measurement) {
            if let Err(error) = std::fs::write(&self.path, text) {
                tracing::warn!(%error, path = %self.path.display(),
                    "capability measurement not persisted");
            }
        }
        *self.latest.write().unwrap() = Some(measurement);
    }

    /// Whether the stored measurement still describes `key`'s world.
    pub fn is_current(&self, key: &MeasurementKey) -> bool {
        self.latest
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|stored| &stored.key == key)
    }
}

/// The application whose capabilities the appliance should measure: the
/// active one when it is a browser, else the first browser app configured.
/// An exec app declares no page and cannot be measured.
pub fn subject(state: &DesiredState) -> Option<AppConfig> {
    let is_browser = |app: &&AppConfig| !matches!(app.launcher, Launcher::Exec { .. });
    state
        .active_app
        .as_ref()
        .and_then(|id| state.apps.iter().filter(is_browser).find(|a| &a.id == id))
        .or_else(|| state.apps.iter().find(is_browser))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kiosk(id: &str) -> AppConfig {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "launcher": { "kind": "chromium-kiosk", "uri": "http://x/" },
        }))
        .unwrap()
    }

    #[test]
    fn the_store_round_trips_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let store = CapabilityStore::new(dir.path());
        assert!(store.latest().is_none());

        let key = MeasurementKey::new(&kiosk("a"), std::path::Path::new("/bin/true"));
        store.record(StoredMeasurement {
            measured_at: 1,
            app_id: "a".into(),
            key: key.clone(),
            report: None,
            note: Some("test".into()),
        });

        let reloaded = CapabilityStore::new(dir.path());
        assert_eq!(reloaded.latest().unwrap().app_id, "a");
        assert!(reloaded.is_current(&key));
    }

    #[test]
    fn the_key_notices_what_matters() {
        let program = std::path::Path::new("/bin/true");
        let key = MeasurementKey::new(&kiosk("a"), program);
        assert!(key == MeasurementKey::new(&kiosk("a"), program), "stable");

        let mut changed = kiosk("a");
        if let Launcher::ChromiumKiosk { extra_args, .. } = &mut changed.launcher {
            extra_args.push("--force-dark-mode".into());
        }
        assert!(key != MeasurementKey::new(&changed, program), "args count");
        assert!(
            key != MeasurementKey::new(&kiosk("a"), std::path::Path::new("/bin/false")),
            "binary counts"
        );
    }

    #[test]
    fn the_subject_is_the_active_browser_or_the_first_one() {
        let mut state = DesiredState::default();
        assert!(subject(&state).is_none(), "nothing configured");

        state.apps.push(
            serde_json::from_value(serde_json::json!({
                "id": "helper", "launcher": { "kind": "exec", "command": "true", "args": [] },
            }))
            .unwrap(),
        );
        assert!(subject(&state).is_none(), "an exec app declares no page");

        state.apps.push(kiosk("first"));
        state.apps.push(kiosk("second"));
        assert_eq!(subject(&state).unwrap().id, "first");

        state.active_app = Some("second".into());
        assert_eq!(subject(&state).unwrap().id, "second", "active app wins");

        state.active_app = Some("helper".into());
        assert_eq!(
            subject(&state).unwrap().id,
            "first",
            "an active exec app falls back to the first browser"
        );
    }
}
