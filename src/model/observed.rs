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

/// Trim a refresh rate to 3 decimals with trailing zeros dropped, the way
/// `sway-output(5)` mode strings and the refresh-rate check both want it
/// formatted. `pub(crate)` rather than private so both can share it instead
/// of drifting apart with their own rounding.
pub(crate) fn format_refresh(hz: f64) -> String {
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
    /// True for sinks that discard whatever is routed to them: the one
    /// Suede manages for silent routing, and the dummy PipeWire falls back
    /// to when it can find no audio devices at all. Never a real output.
    pub is_null_sink: bool,
    /// True when this is PipeWire's current default sink.
    pub is_default: bool,
    /// Video connector this sink is associated with, where derivable.
    pub output_hint: Option<String>,
    /// Current playback gain in dB, `0.0` being unity. `None` when PipeWire
    /// reports no volume for this sink at all.
    ///
    /// Derived from the linear `channelVolumes` PipeWire holds, so it is the
    /// gain actually in the signal path rather than the cube-rooted number a
    /// mixer UI displays.
    pub gain_db: Option<f64>,
}

/// An audio input (capture) device reported by PipeWire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioSource {
    /// PipeWire `node.name` — stable across reboots and replugging.
    pub id: String,
    /// Human-readable `node.description`. This is what a Chromium kiosk's
    /// `enumerateDevices` labels the device as, since audio devices are
    /// opened through pipewire-pulse.
    pub description: Option<String>,
    /// ALSA card name (`api.alsa.card.name`), where PipeWire reports one.
    pub card: Option<String>,
}

/// A video input (capture) device, surfaced via WirePlumber's v4l2 monitor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoSource {
    /// PipeWire `node.name` — stable across reboots and replugging.
    pub id: String,
    /// Human-readable `node.description`.
    pub description: Option<String>,
    /// V4L2 device path (`api.v4l2.path`), e.g. `/dev/video0`.
    pub path: Option<String>,
    /// V4L2 card string (`api.v4l2.cap.card`). This, not `description`, is
    /// what a Chromium kiosk's `enumerateDevices` labels the device as,
    /// since video devices are opened straight through V4L2.
    pub card: Option<String>,
}

/// Every audio and video device PipeWire currently reports: outputs Suede
/// can route to, and inputs an app might ask to be given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct AvDevices {
    /// Audio sinks. The only list with a notion of default.
    pub audio_outputs: Vec<AudioSink>,
    /// Audio sources (`Audio/Source` nodes).
    pub audio_inputs: Vec<AudioSource>,
    /// Video sources (`Video/Source` nodes), from WirePlumber's v4l2 monitor.
    pub video_inputs: Vec<VideoSource>,
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
        "sway_unreachable",
        "output_not_connected",
        "mode_unsupported",
        "command_failed",
        "app_waiting_for_output",
        "app_crash_looping",
        "app_halted",
        "app_output_disabled",
        "app_program_not_allowed",
        "audio_sink_not_present",
        "null_sink_unavailable",
        "wallpaper_not_found",
        "background_preset_not_found",
        "tearing_unsupported",
        "projection_unavailable",
        "blend_overlay_failed",
        "headless_unavailable",
    ];

    /// Documentation page for a divergence kind, relative to the docs root.
    pub fn docs_path(kind: &str) -> Option<&'static str> {
        Some(match kind {
            "sway_unreachable" => "troubleshooting/#no-sway-ipc-socket-found",
            "output_not_connected" | "mode_unsupported" | "command_failed" => {
                "troubleshooting/#a-display-stays-dark"
            }
            "app_waiting_for_output"
            | "app_output_disabled"
            | "app_crash_looping"
            | "app_halted" => "troubleshooting/#a-browser-will-not-start",
            "app_program_not_allowed" => "troubleshooting/#program-not-allowed",
            "audio_sink_not_present" | "null_sink_unavailable" => {
                "troubleshooting/#audio-goes-to-the-wrong-place-or-nowhere"
            }
            "wallpaper_not_found" | "background_preset_not_found" => {
                "configuration/#backgrounds-and-wallpapers"
            }
            "tearing_unsupported" => "configuration/#outputs",
            "projection_unavailable" | "blend_overlay_failed" | "headless_unavailable" => {
                "configuration/#projection-edge-blending"
            }
            _ => return None,
        })
    }
}

/// Reconciliation status, as served by `GET /status`.
///
/// Answers "is this appliance doing what I asked, and will it still be after
/// a reboot" from one call: `state` and `divergences` alone say whether
/// desired state is fulfilled *right now*, but say nothing about whether the
/// document behind that answer survives a restart, whether the machine has
/// caught up with a write still in flight, or whether the environment it
/// depends on is otherwise healthy. See `docs/specification.md`'s `/status`
/// section for the predicate spelled out for client authors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: SyncState,
    pub divergences: Vec<Divergence>,
    /// Unix seconds of the last completed reconciliation pass.
    pub last_reconciled: Option<u64>,
    /// Desired-state revision the last pass applied.
    pub revision: u64,
    /// Whether the document the last pass applied was the saved one.
    ///
    /// A working copy is applied exactly as a saved document is, so an
    /// appliance can be `synced` against a document that will vanish on
    /// restart. A client asking "will it still look like this tomorrow"
    /// needs this, and it is not otherwise visible without fetching the
    /// configuration too.
    pub committed: bool,
    /// The revision of the desired-state document right now.
    ///
    /// `revision` is the one the last pass *applied*; these differ while a
    /// write is still being reconciled. Equal values mean the machine has
    /// caught up with the last write, which is the question a client asks
    /// after a PUT.
    pub current_revision: u64,
    /// How the environment health checks last came out, by status.
    ///
    /// Divergences and checks are different axes: a machine can apply its
    /// configuration perfectly while its browser is missing hardware video
    /// decode. Summarised here so the common question takes one call; the
    /// detail stays at `GET /system/checks`.
    ///
    /// `None` rather than an all-zero [`CheckSummary`] wherever the counts
    /// are not actually known. A reconciliation pass never runs the checks
    /// itself — they shell out to other programs, and a pass must stay
    /// cheap — so the `Status` it publishes over SSE cannot honestly fill
    /// this in; a zeroed summary there would read as "everything passed",
    /// which is a claim nobody made. `GET /status` always fills it in from
    /// the check runner's last results, because it has one to ask.
    pub checks: Option<CheckSummary>,
    /// The app `activeApp` names, and how it is doing, so "is my app on the
    /// screens" is one call: `state == running` here beside `synced` above.
    /// `None` when no app is active. Full detail (pid, restarts, window ids)
    /// stays on `GET /apps/{id}/status`.
    pub active_app: Option<ActiveApp>,
}

/// What `Status.activeApp` says about the one app the appliance is showing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActiveApp {
    pub id: String,
    pub state: AppState,
    /// Human-readable detail for the current state, mirroring
    /// [`AppStatus::detail`].
    pub detail: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            state: SyncState::default(),
            divergences: Vec::new(),
            last_reconciled: None,
            revision: 0,
            // An appliance that has reconciled nothing yet has no working
            // copy to distrust either, so the honest answer to "will this
            // survive a reboot" is yes — there is only the (empty) saved
            // document.
            committed: true,
            current_revision: 0,
            checks: None,
            active_app: None,
        }
    }
}

/// How the environment health checks last came out, tallied by status.
///
/// The field names are exactly [`CheckStatus`]'s variants: no second
/// vocabulary for the same three words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CheckSummary {
    pub pass: u32,
    pub warn: u32,
    pub fail: u32,
}

impl CheckSummary {
    /// Tally a set of check results by status.
    ///
    /// `None` for an empty slice rather than an all-zero summary: the two
    /// are indistinguishable in the counts alone, but mean opposite things to
    /// a client — "every check passed" versus "no check has ever run", which
    /// is exactly the state at daemon startup, before the first scheduled
    /// pass.
    pub fn tally(checks: &[Check]) -> Option<Self> {
        if checks.is_empty() {
            return None;
        }
        let mut summary = Self::default();
        for check in checks {
            match check.status {
                CheckStatus::Pass => summary.pass += 1,
                CheckStatus::Warn => summary.warn += 1,
                CheckStatus::Fail => summary.fail += 1,
            }
        }
        Some(summary)
    }
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
    /// Which build is running: `git describe` output, e.g.
    /// `v0.1.0-12-g81226ee`. `suedeVersion` names the release this is meant
    /// to be; this names the commit it was actually built from, which is the
    /// question when a fix appears not to have landed.
    pub build_id: String,
    pub sway_version: Option<String>,
    pub hostname: Option<String>,
    /// Seconds since the daemon started.
    pub uptime_seconds: u64,
    pub packages: Vec<PackageVersion>,
    /// Feature gates resolved from the detected Sway version.
    pub supports_tearing: bool,
    /// True when the reference web UI is being served.
    pub web_ui_enabled: bool,
    /// Host power operations this appliance permits. Empty means none; the web
    /// UI uses it to disable and explain its buttons rather than offering ones
    /// that would be refused.
    pub power_verbs: Vec<PowerVerb>,
}

/// A host power operation Suede may be permitted to perform.
///
/// Lives here, not in `desired`, because `GET /system` reports it (so it
/// needs [`ToSchema`]) and it is read by [`crate::config::BootstrapConfig`],
/// which cannot depend on desired state without an awkward cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PowerVerb {
    Reboot,
    Poweroff,
}

impl PowerVerb {
    /// Every accepted verb, in the order an error message should list them.
    pub const ALL: &'static [PowerVerb] = &[PowerVerb::Reboot, PowerVerb::Poweroff];

    /// The lowercase spelling used in TOML, `SUEDE_POWER`, and as the
    /// `systemctl` subcommand — the three places this string travels all
    /// happen to want exactly the same word.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reboot => "reboot",
            Self::Poweroff => "poweroff",
        }
    }

    /// Parse the lowercase spelling. `None` rather than an error type of its
    /// own: every caller that cares about a bad value wants to name it and
    /// list [`PowerVerb::ALL`] itself, which needs the original string.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reboot" => Some(Self::Reboot),
            "poweroff" => Some(Self::Poweroff),
            _ => None,
        }
    }
}

/// Result of a single environment health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// What a browser measured about itself, posted back by the capability page.
///
/// Observed state like any other, except the observer is the browser: these
/// are the media APIs' own answers from inside the operator's exact
/// configuration, not an inspection from outside it. The page constructs
/// exactly this shape; anything else is drift between the two halves and is
/// rejected loudly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityReport {
    pub user_agent: String,
    /// From `WEBGL_debug_renderer_info`. `null` when WebGL is unavailable or
    /// the browser masks it — itself a finding, since a software rasteriser
    /// usually announces itself here (`SwiftShader`, `llvmpipe`).
    #[serde(default)]
    pub gpu_vendor: Option<String>,
    #[serde(default)]
    pub gpu_renderer: Option<String>,
    /// Whether a WebGPU adapter was obtainable.
    pub webgpu: bool,
    /// Whether the WebCodecs `VideoDecoder` API exists at all — without it
    /// the per-codec `hardware` column cannot be measured.
    pub video_decoder_api: bool,
    /// Anything the page could not measure, in its own words.
    pub notes: Vec<String>,
    pub codecs: Vec<CodecSupport>,
}

/// One codec at one resolution, as the media APIs answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodecSupport {
    /// Human label, e.g. `H.265 Main 2160p60`.
    pub label: String,
    /// What was actually asked for, e.g. `video/mp4; codecs="hvc1.1.6.L153.B0"`.
    pub content_type: String,
    /// `MediaCapabilities.decodingInfo().supported`.
    pub supported: bool,
    #[serde(default)]
    pub smooth: Option<bool>,
    /// The classic hardware-decode signal; browsers report it conservatively.
    #[serde(default)]
    pub power_efficient: Option<bool>,
    /// `VideoDecoder.isConfigSupported` with `prefer-hardware`: `true` means
    /// a hardware decoder accepted the configuration, `false` means only a
    /// software one did, `null` means the API could not answer.
    #[serde(default)]
    pub hardware: Option<bool>,
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

/// What the projection pipeline is doing right now, as served by `GET
/// /projection/stats` and `projection_stats_changed`.
///
/// Added 2026-09-15: a four-projector bench had a slicer that was
/// demonstrably alive (`pgrep -f "suede slice"` found it, and it had logged
/// its startup line) but had produced no frames, because the frame loop is
/// damage-driven and the active page was a static image. The endpoint used
/// to be `ProjectionStats | null`, and that `null` was reported for "no
/// slicer at all" and "slicer running but silent" alike — which is exactly
/// how a running slicer got diagnosed as not running. `running` is tracked
/// separately from whatever the slicer has or has not reported, so the two
/// situations are no longer the same value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionReport {
    /// Whether a slicer process is alive. A slicer can be running and yet
    /// have reported nothing: the frame loop is damage-driven, so a static
    /// page produces no frames and therefore no interval. Distinguishing
    /// that from "no slicer at all" is the point of this field — the two
    /// were indistinguishable until a bench reported "the slicer is not
    /// running" about a slicer that was running.
    pub running: bool,
    /// The last completed reporting interval, or `null` when none has been
    /// produced yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_interval: Option<ProjectionStats>,
}

/// What the slicer measured over its last reporting interval.
///
/// Reported by the slicer subprocess as a JSON line on its stdout every ten
/// seconds (see `crate::projection::slicer`), read by the manager and held
/// as [`ProjectionReport::last_interval`]. `None` there means no interval
/// has completed yet, which is normal for a slicer that is running but has
/// nothing to capture — see [`ProjectionReport::running`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionStats {
    /// Unix seconds at the end of the interval.
    pub measured_at: u64,
    pub interval_seconds: f64,
    /// Whether outputs were taking frames at their own pace (see ProjectionConfig.free_run).
    pub free_run: bool,
    /// Canvas frames captured per second.
    pub canvas_fps: f64,
    /// Present cycles per second. Locked: one per all-output commit. Free: one per
    /// snapshot that reached at least one output.
    pub presented_fps: f64,
    /// Captured frames replaced by a newer one before any output showed them.
    pub frames_superseded: u32,
    /// Times an output stopped answering frame callbacks and was dropped from the gate.
    pub stalls: u32,
    pub per_frame_ms: FrameCost,
    /// Whether the compositor offers wp_presentation; without it offset_ms and the
    /// per-output presented/discarded/refreshHz fields cannot be measured.
    pub presentation_feedback: bool,
    /// Spread between the earliest and latest output to present the same frame.
    /// None when fewer than two outputs reported a frame this interval.
    pub offset_ms: Option<PresentationOffset>,
    /// Frames whose outputs presented more than half a refresh period apart —
    /// i.e. shown on different refreshes, a whole-frame mismatch.
    pub straddles: u32,
    /// Which backend this interval's frames were blended on: `"cpu"` or `"gpu"`.
    pub renderer: String,
    /// How many canvas periods elapsed between successive captures reaching
    /// the slicer, bucketed. Says directly whether the capture loop is
    /// keeping up with every canvas frame or only every second one.
    pub capture_intervals: CaptureIntervals,
    pub outputs: Vec<OutputTiming>,
}

/// Where a captured frame's time went in the slicer, in ms, averaged over
/// the reporting interval. See `crate::projection::slicer::FrameStats` for
/// what each phase covers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameCost {
    pub waiting: f64,
    pub snapshot: f64,
    pub requesting: f64,
    pub blending: f64,
    /// GPU fence wait per frame — see `crate::projection::gpu::Gpu::blend`.
    /// Zero on the CPU path, which never waits on a fence.
    pub gpu: f64,
}

/// How many canvas periods elapsed between one capture reaching the slicer
/// and the previous one, bucketed over the reporting interval. A period is
/// `1000 / canvas refresh` when the canvas output reports one, else the
/// 16.667 ms of an assumed 60 Hz. A healthy capture loop that keeps up with
/// every canvas frame counts almost entirely in `one`; a loop that can only
/// manage every second frame (the four-projector rig's CPU path before the
/// GPU one existed — see `crate::projection::gpu`'s module doc) counts
/// almost entirely in `two`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CaptureIntervals {
    pub one: u32,
    pub two: u32,
    pub three: u32,
    /// More than 3.5 periods since the previous capture.
    pub more: u32,
}

/// Spread between the earliest and latest output to present the same frame,
/// from `wp_presentation` feedback, in ms.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresentationOffset {
    pub mean: f64,
    pub max: f64,
}

/// One output's presentation tally for the interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutputTiming {
    pub name: String,
    pub presented: u32,
    pub discarded: u32,
    /// How many of `presented` carried the presentation feedback's
    /// `zero_copy` flag: the compositor scanned this output's buffer out
    /// directly to the display controller, with no compositing pass, rather
    /// than blitting it into its own framebuffer first.
    pub zero_copy_presented: u32,
    /// From wp_presentation's refresh field: the output's actual refresh interval, as Hz.
    pub refresh_hz: Option<f64>,
    /// This output's vblank phase relative to the first output, in ms, within
    /// half a refresh period either side. Stable across intervals means the
    /// heads are locked at a fixed offset; wandering means independent clocks.
    pub phase_ms: Option<f64>,
    /// The spread of that phase within the interval (max − min of the
    /// per-frame value), ms. Near zero means locked; near a period means drifting.
    pub phase_spread_ms: Option<f64>,
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
    fn status_serialises_the_new_fields_in_camel_case() {
        let status = Status {
            state: SyncState::Synced,
            divergences: vec![],
            last_reconciled: Some(10),
            revision: 3,
            committed: false,
            current_revision: 4,
            checks: Some(CheckSummary {
                pass: 1,
                warn: 2,
                fail: 3,
            }),
            active_app: Some(ActiveApp {
                id: "renderer".into(),
                state: AppState::Running,
                detail: None,
            }),
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["committed"], false);
        assert_eq!(value["currentRevision"], 4);
        assert_eq!(value["checks"]["pass"], 1);
        assert_eq!(value["checks"]["warn"], 2);
        assert_eq!(value["checks"]["fail"], 3);
        assert_eq!(value["activeApp"]["id"], "renderer");
        assert_eq!(value["activeApp"]["state"], "running");
    }

    #[test]
    fn status_serialises_active_app_as_null_when_absent() {
        let status = Status::default();
        let value = serde_json::to_value(&status).unwrap();
        assert!(
            value.get("activeApp").is_some(),
            "must be present, not skipped"
        );
        assert!(value["activeApp"].is_null());
    }

    #[test]
    fn status_default_is_committed_with_no_checks_yet_reported() {
        let status = Status::default();
        assert!(
            status.committed,
            "an empty document has no working copy to distrust"
        );
        assert_eq!(status.current_revision, 0);
        assert!(
            status.checks.is_none(),
            "nothing has run from a bare default"
        );
    }

    #[test]
    fn check_summary_tally_distinguishes_never_run_from_all_pass() {
        assert!(
            CheckSummary::tally(&[]).is_none(),
            "no checks run is not the same claim as zero failures"
        );

        let sample = |status: CheckStatus| Check {
            id: "x".into(),
            title: "X".into(),
            status,
            detail: String::new(),
            docs_url: None,
            fix_available: false,
            fix_description: None,
        };
        let checks = vec![
            sample(CheckStatus::Pass),
            sample(CheckStatus::Pass),
            sample(CheckStatus::Warn),
            sample(CheckStatus::Fail),
        ];
        assert_eq!(
            CheckSummary::tally(&checks),
            Some(CheckSummary {
                pass: 2,
                warn: 1,
                fail: 1
            })
        );
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
