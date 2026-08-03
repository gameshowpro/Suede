//! Observed state: what Sway, PipeWire, and the supervisor currently report.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How far an advertised refresh rate may sit from a requested one and still
/// be considered the mode the client meant. Wide enough for EDID jitter
/// (59.81–60.02 for "60"), narrow enough to never confuse 50, 60, 72 or 75.
pub const REFRESH_TOLERANCE_HZ: f64 = 1.0;

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
        self.resolve_mode(mode).is_some()
    }

    /// The advertised mode that best satisfies `wanted`, if any.
    ///
    /// Real EDIDs almost never carry round refresh rates: a display offering
    /// "1440p60" actually advertises 59.951 Hz, and 4K60 is 59.997 Hz. Asking
    /// for 60 must therefore select the nearest rate at that resolution rather
    /// than being refused — while still keeping genuinely distinct rates (50,
    /// 60, 72, 75) apart.
    pub fn resolve_mode(&self, wanted: &Mode) -> Option<Mode> {
        if let Some(exact) = self.modes.iter().find(|m| m.matches(wanted)) {
            return Some(*exact);
        }
        self.modes
            .iter()
            .filter(|m| m.width == wanted.width && m.height == wanted.height)
            .filter(|m| (m.refresh_hz - wanted.refresh_hz).abs() <= REFRESH_TOLERANCE_HZ)
            .min_by(|a, b| {
                let da = (a.refresh_hz - wanted.refresh_hz).abs();
                let db = (b.refresh_hz - wanted.refresh_hz).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
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
///
/// Values are camelCase like the rest of the API: `lowercase` would render
/// `WaitingForOutput` as the unreadable `waitingforoutput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
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
    /// Enabled, but the URL it depends on is not answering yet.
    WaitingForDependency,
}

/// Why an app was last restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
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
    /// Where to read about this kind of problem.
    ///
    /// Carried here, rather than left for each client to work out, so that
    /// anything consuming the API can offer the same help the bundled UI does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

impl Divergence {
    pub fn new(kind: &str, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            subject: subject.into(),
            detail: detail.into(),
            docs_url: None,
        }
    }

    /// Every kind the reconciler can raise.
    ///
    /// Listed so a test can prove each one leads the operator somewhere; a new
    /// kind without documentation is a dead end in the UI.
    pub const KINDS: &'static [&'static str] = &[
        "output_not_connected",
        "mode_unsupported",
        "command_failed",
        "app_waiting_for_output",
        "app_output_disabled",
        "audio_sink_not_present",
        "null_sink_unavailable",
        "wallpaper_not_found",
        "background_preset_not_found",
        "tearing_unsupported",
        "projection_output_not_found",
        "projection_unavailable",
        "blend_overlay_failed",
    ];

    /// Documentation page for a divergence kind, relative to the docs root.
    pub fn docs_path(kind: &str) -> Option<&'static str> {
        Some(match kind {
            "output_not_connected" | "mode_unsupported" | "command_failed" => {
                "troubleshooting/#a-display-stays-dark"
            }
            "app_waiting_for_output" | "app_output_disabled" => {
                "troubleshooting/#a-browser-will-not-start"
            }
            "audio_sink_not_present" | "null_sink_unavailable" => {
                "troubleshooting/#audio-goes-to-the-wrong-place-or-nowhere"
            }
            "wallpaper_not_found" | "background_preset_not_found" => {
                "configuration/#backgrounds-and-wallpapers"
            }
            "tearing_unsupported" => "configuration/#outputs",
            "projection_output_not_found" | "projection_unavailable" | "blend_overlay_failed" => {
                "configuration/#projection-edge-blending"
            }
            _ => return None,
        })
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

    /// Refresh rates taken verbatim from a Samsung U28E510's EDID.
    fn real_display() -> Output {
        Output {
            name: "DP-3".into(),
            active: true,
            make: Some("Samsung Electric Company".into()),
            model: Some("U28E510".into()),
            serial: None,
            current_mode: None,
            modes: vec![
                Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 59.939,
                },
                Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60.000,
                },
                Mode {
                    width: 2560,
                    height: 1440,
                    refresh_hz: 59.951,
                },
                Mode {
                    width: 3840,
                    height: 2160,
                    refresh_hz: 59.997,
                },
                Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60.000,
                },
                Mode {
                    width: 640,
                    height: 480,
                    refresh_hz: 59.940,
                },
                Mode {
                    width: 640,
                    height: 480,
                    refresh_hz: 72.809,
                },
                Mode {
                    width: 640,
                    height: 480,
                    refresh_hz: 75.000,
                },
            ],
            rect: Rect::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    #[test]
    fn enum_values_stay_readable_in_json() {
        // `lowercase` would produce "waitingforoutput"; the docs and the UI
        // both expect camelCase.
        assert_eq!(
            serde_json::to_value(AppState::WaitingForOutput).unwrap(),
            "waitingForOutput"
        );
        assert_eq!(serde_json::to_value(AppState::Running).unwrap(), "running");
        assert_eq!(
            serde_json::to_value(RestartReason::HeartbeatTimeout).unwrap(),
            "heartbeatTimeout"
        );
    }

    #[test]
    fn a_request_for_60_finds_the_real_rate_beside_it() {
        // No real display advertises exactly 60 at 1440p; this one says 59.951.
        let display = real_display();
        let resolved = display
            .resolve_mode(&Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 60.0,
            })
            .expect("2560x1440 is advertised and must resolve");
        assert_eq!(resolved.refresh_hz, 59.951);
        assert!(display.supports(&Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60.0
        }));
    }

    #[test]
    fn an_exact_rate_is_preferred_over_a_near_one() {
        // 1080p is advertised at both 59.939 and 60.000.
        let resolved = real_display()
            .resolve_mode(&Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0,
            })
            .unwrap();
        assert_eq!(resolved.refresh_hz, 60.0);
    }

    #[test]
    fn distinct_refresh_rates_are_never_conflated() {
        let display = real_display();
        // 640x480 offers 59.94, 72.809 and 75; each request must land on its own.
        for (asked, expected) in [(60.0, 59.940), (72.0, 72.809), (75.0, 75.000)] {
            let resolved = display
                .resolve_mode(&Mode {
                    width: 640,
                    height: 480,
                    refresh_hz: asked,
                })
                .unwrap();
            assert_eq!(resolved.refresh_hz, expected, "asking for {asked}");
        }
        // 50 Hz is not on offer at all and must not borrow the 59.94 mode.
        assert!(display
            .resolve_mode(&Mode {
                width: 640,
                height: 480,
                refresh_hz: 50.0
            })
            .is_none());
    }

    #[test]
    fn an_unavailable_resolution_still_resolves_to_nothing() {
        assert!(real_display()
            .resolve_mode(&Mode {
                width: 7680,
                height: 4320,
                refresh_hz: 60.0
            })
            .is_none());
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

    #[test]
    fn every_divergence_kind_leads_somewhere() {
        // A divergence with no fix and no documentation leaves the operator
        // holding a complaint and nothing to do about it.
        for kind in Divergence::KINDS {
            assert!(
                Divergence::docs_path(kind).is_some(),
                "{kind} has no documentation page"
            );
        }
        assert_eq!(Divergence::docs_path("invented_kind"), None);
    }
}
