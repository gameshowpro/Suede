//! Observed state: what Sway, PipeWire, and the supervisor currently report.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A display mode. Refresh is in Hz (Sway reports mHz on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    #[serde(rename = "refreshHz")]
    pub refresh_hz: f64,
}

impl Mode {
    /// Compare modes the way Sway effectively does: exact pixels, refresh to 0.01 Hz.
    pub fn matches(&self, other: &Mode) -> bool {
        self.width == other.width
            && self.height == other.height
            && (self.refresh_hz - other.refresh_hz).abs() < 0.01
    }

    /// `1920x1080@60Hz`, formatted the way `sway-output(5)` expects.
    pub fn to_sway(self) -> String {
        format!(
            "{}x{}@{}Hz",
            self.width,
            self.height,
            format_refresh(self.refresh_hz)
        )
    }
}

fn format_refresh(hz: f64) -> String {
    let rounded = (hz * 1000.0).round() / 1000.0;
    let text = format!("{rounded:.3}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A video output as reported by Sway's `get_outputs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    /// Connector name, e.g. `HDMI-A-1`.
    pub name: String,
    /// Whether Sway currently has this output enabled.
    pub active: bool,
    /// EDID manufacturer.
    pub make: Option<String>,
    /// EDID model.
    pub model: Option<String>,
    /// EDID serial, where the display provides one.
    pub serial: Option<String>,
    /// Mode currently applied.
    pub current_mode: Option<Mode>,
    /// All modes the display advertises, deduplicated.
    pub modes: Vec<Mode>,
    /// Position and size in the global layout.
    pub rect: Rect,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub adaptive_sync_status: Option<String>,
}

impl Output {
    /// Highest-resolution, then highest-refresh mode, useful as a sensible default.
    pub fn maximum_mode(&self) -> Option<Mode> {
        self.modes.iter().copied().max_by(|a, b| {
            let area = (a.width as i64 * a.height as i64).cmp(&(b.width as i64 * b.height as i64));
            area.then(
                a.refresh_hz
                    .partial_cmp(&b.refresh_hz)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        })
    }

    /// Whether this output advertises a mode compatible with `mode`.
    pub fn supports(&self, mode: &Mode) -> bool {
        self.modes.iter().any(|m| m.matches(mode))
    }
}

/// A window in Sway's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// Sway container id.
    pub id: i64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub pid: Option<i32>,
    pub visible: Option<bool>,
    pub fullscreen_mode: i32,
    pub rect: Rect,
    /// Name of the output this window is displayed on, where known.
    pub output: Option<String>,
    /// Id of the Suede-managed app that owns this window, where known.
    pub app: Option<String>,
}

/// Lifecycle state of a supervised application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AppState {
    /// Spawned; waiting for its window to appear.
    Starting,
    /// Running with a window mapped.
    Running,
    /// Not running because it is disabled or was removed.
    Stopped,
    /// Exited unexpectedly, or was killed by the watchdog.
    Crashed,
    /// Waiting out a restart delay before the next attempt.
    Backoff,
    /// Enabled, but its target output is not currently connected.
    WaitingForOutput,
}

/// Why an app was last restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestartReason {
    ProcessExited,
    HeartbeatTimeout,
    WindowNeverAppeared,
    ConfigChanged,
    ApiRequest,
}

/// Runtime status of a supervised application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub id: String,
    pub state: AppState,
    pub pid: Option<u32>,
    /// Unix seconds when the current process was spawned.
    pub started_at: Option<u64>,
    pub restart_count: u32,
    /// Sway container ids currently attributed to this app.
    pub window_ids: Vec<i64>,
    /// Unix seconds of the most recent heartbeat, when the watchdog is enabled.
    pub last_heartbeat: Option<u64>,
    pub last_exit_code: Option<i32>,
    pub last_restart_reason: Option<RestartReason>,
    /// Human-readable detail for the current state.
    pub detail: Option<String>,
}

impl AppStatus {
    pub fn stopped(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: AppState::Stopped,
            pid: None,
            started_at: None,
            restart_count: 0,
            window_ids: Vec::new(),
            last_heartbeat: None,
            last_exit_code: None,
            last_restart_reason: None,
            detail: None,
        }
    }
}

/// An audio sink reported by PipeWire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioSink {
    /// PipeWire `node.name` — stable across reboots and replugging.
    pub id: String,
    /// Human-readable `node.description`.
    pub description: Option<String>,
    /// True for the null sink Suede manages for silent routing.
    pub is_null_sink: bool,
    /// True when this is PipeWire's current default sink.
    pub is_default: bool,
    /// Video connector this sink is associated with, where derivable.
    pub output_hint: Option<String>,
}

/// Overall reconciliation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    /// Live state matches desired state.
    #[default]
    Synced,
    /// A reconciliation pass is in flight.
    Reconciling,
    /// Desired state could not be fully realized.
    Degraded,
}

/// Something desired that could not be realized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Divergence {
    /// Machine-readable kind, e.g. `output_not_connected`.
    pub kind: String,
    /// Resource the divergence concerns, e.g. an output name or app id.
    pub subject: String,
    /// Human-readable explanation.
    pub detail: String,
}

impl Divergence {
    pub fn new(kind: &str, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            subject: subject.into(),
            detail: detail.into(),
        }
    }
}

/// Reconciliation status, as served by `GET /status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: SyncState,
    pub divergences: Vec<Divergence>,
    /// Unix seconds of the last completed reconciliation pass.
    pub last_reconciled: Option<u64>,
    /// Desired-state revision the last pass applied.
    pub revision: u64,
}

/// Version of a package relevant to Suede's operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackageVersion {
    pub name: String,
    pub version: Option<String>,
}

/// Daemon and environment information, as served by `GET /system`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub suede_version: String,
    pub sway_version: Option<String>,
    pub hostname: Option<String>,
    /// Seconds since the daemon started.
    pub uptime_seconds: u64,
    pub packages: Vec<PackageVersion>,
    /// Feature gates resolved from the detected Sway version.
    pub supports_tearing: bool,
    /// True when the reference web UI is being served.
    pub web_ui_enabled: bool,
}

/// Result of a single environment health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// An environment health check, as served by `GET /system/checks`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    /// Stable identifier, e.g. `sway-socket`.
    pub id: String,
    /// Short human-readable name.
    pub title: String,
    pub status: CheckStatus,
    /// Explanation of the current result.
    pub detail: String,
    /// Link to the documentation page describing manual resolution.
    pub docs_url: Option<String>,
    /// Whether `POST /system/checks/{id}/fix` can remediate this check.
    pub fix_available: bool,
    /// What the fix would do, shown before it is invoked.
    pub fix_description: Option<String>,
}

/// Payload of the `windows_changed` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WindowChange {
    /// Sway change type: `new`, `close`, `title`, `move`, `fullscreen_mode`, `floating`.
    pub change: String,
    pub window: Window,
}

/// Payload of the `config_changed` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChange {
    pub revision: u64,
    /// Which part of the document changed: `all`, `outputs`, `apps`, `settings`.
    pub section: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_formats_for_sway() {
        assert_eq!(
            Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0
            }
            .to_sway(),
            "1920x1080@60Hz"
        );
        assert_eq!(
            Mode {
                width: 3840,
                height: 2160,
                refresh_hz: 59.997,
            }
            .to_sway(),
            "3840x2160@59.997Hz"
        );
    }

    #[test]
    fn mode_matching_tolerates_rounding() {
        let a = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        };
        let b = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.001,
        };
        assert!(a.matches(&b));
        let c = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 50.0,
        };
        assert!(!a.matches(&c));
    }

    #[test]
    fn maximum_mode_prefers_area_then_refresh() {
        let output = Output {
            name: "HDMI-A-1".into(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![
                Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60.0,
                },
                Mode {
                    width: 3840,
                    height: 2160,
                    refresh_hz: 30.0,
                },
                Mode {
                    width: 3840,
                    height: 2160,
                    refresh_hz: 60.0,
                },
            ],
            rect: Rect::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        };
        let max = output.maximum_mode().unwrap();
        assert_eq!((max.width, max.height), (3840, 2160));
        assert_eq!(max.refresh_hz, 60.0);
    }
}
