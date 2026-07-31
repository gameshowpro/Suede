//! Desired state: the document clients write and Suede persists.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::observed::{Mode, Output, Position};

/// Schema version of the persisted document. Bump when a migration is needed.
pub const SCHEMA_VERSION: u32 = 1;

/// The complete desired-state document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DesiredState {
    /// Document schema version, managed by Suede.
    pub schema_version: u32,
    /// Monotonic revision, incremented by Suede on every accepted write.
    pub revision: u64,
    pub outputs: Vec<OutputConfig>,
    pub apps: Vec<AppConfig>,
    pub settings: Settings,
}

impl DesiredState {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        }
    }

    /// Semantic validation beyond what serde enforces. Returns all problems found.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        let mut seen_outputs = std::collections::HashSet::new();
        for (index, output) in self.outputs.iter().enumerate() {
            let prefix = format!("outputs[{index}]");
            if output.r#match.is_empty() {
                errors.push(format!(
                    "{prefix}.match must set at least one of name, make, model, serial"
                ));
            }
            if !seen_outputs.insert(output.r#match.key()) {
                errors.push(format!(
                    "{prefix}.match duplicates an earlier entry ({})",
                    output.r#match.key()
                ));
            }
            if let Some(mode) = &output.mode {
                if mode.width <= 0 || mode.height <= 0 {
                    errors.push(format!("{prefix}.mode dimensions must be positive"));
                }
                if mode.refresh_hz <= 0.0 {
                    errors.push(format!("{prefix}.mode.refreshHz must be positive"));
                }
            }
            if let Some(scale) = output.scale {
                if !(scale.is_finite() && scale > 0.0) {
                    errors.push(format!("{prefix}.scale must be a positive number"));
                }
            }
            if let Some(background) = &output.background {
                if let Some(color) = &background.color {
                    let digits = color.trim_start_matches('#');
                    let valid = matches!(digits.len(), 6 | 8)
                        && digits.chars().all(|c| c.is_ascii_hexdigit());
                    if !valid {
                        errors.push(format!(
                            "{prefix}.background.color {color:?} must be #rrggbb or #rrggbbaa"
                        ));
                    }
                }
                if let Some(id) = &background.wallpaper {
                    if id.is_empty() || id.contains("..") || id.contains('/') {
                        errors.push(format!(
                            "{prefix}.background.wallpaper {id:?} is not a valid wallpaper id"
                        ));
                    }
                }
            }
        }

        let mut seen_apps = std::collections::HashSet::new();
        for (index, app) in self.apps.iter().enumerate() {
            let prefix = format!("apps[{index}]");
            if app.id.trim().is_empty() {
                errors.push(format!("{prefix}.id must not be empty"));
            } else if !app
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                errors.push(format!(
                    "{prefix}.id {:?} may only contain letters, digits, '-', '_' and '.'",
                    app.id
                ));
            }
            // The id becomes a directory name under the state directory.
            if app.id.contains("..") || app.id == "." {
                errors.push(format!(
                    "{prefix}.id {:?} must not be a relative path fragment",
                    app.id
                ));
            }
            if !seen_apps.insert(app.id.clone()) {
                errors.push(format!("{prefix}.id {:?} is not unique", app.id));
            }
            match &app.launcher {
                Launcher::ChromiumKiosk { uri, .. } | Launcher::FirefoxKiosk { uri, .. } => {
                    if uri.trim().is_empty() {
                        errors.push(format!("{prefix}.launcher.uri must not be empty"));
                    }
                }
                Launcher::Exec { command, .. } => {
                    if command.trim().is_empty() {
                        errors.push(format!("{prefix}.launcher.command must not be empty"));
                    }
                }
            }
            if let Some(output) = &app.output {
                if output.is_empty() {
                    errors.push(format!(
                        "{prefix}.output must set at least one of name, make, model, serial"
                    ));
                }
            }
            if app.restart.delay_ms > app.restart.max_delay_ms {
                errors.push(format!(
                    "{prefix}.restart.delayMs must not exceed maxDelayMs"
                ));
            }
            if let Some(heartbeat) = &app.heartbeat {
                if heartbeat.enabled && heartbeat.timeout_seconds == 0 {
                    errors.push(format!(
                        "{prefix}.heartbeat.timeoutSeconds must be greater than zero"
                    ));
                }
            }
            for key in app.env.keys() {
                if key.is_empty() || key.contains('=') || key.contains('\0') {
                    errors.push(format!("{prefix}.env has an invalid variable name {key:?}"));
                }
            }
            if let Some(readiness) = &app.readiness {
                // Caught here rather than at launch, where a bad URL would
                // present as an application that simply never starts.
                if let Err(error) = crate::probe::parse_url(&readiness.url) {
                    errors.push(format!("{prefix}.readiness.url {error}"));
                }
                if readiness.interval_seconds == 0 {
                    errors.push(format!(
                        "{prefix}.readiness.intervalSeconds must be greater than zero"
                    ));
                }
                if readiness.timeout_seconds == 0 {
                    errors.push(format!(
                        "{prefix}.readiness.timeoutSeconds must be greater than zero"
                    ));
                }
            }
        }

        if self.settings.output_poll_interval_seconds == 0 {
            errors.push("settings.outputPollIntervalSeconds must be greater than zero".into());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn app(&self, id: &str) -> Option<&AppConfig> {
        self.apps.iter().find(|a| a.id == id)
    }
}

/// Rule selecting which physical output a config entry applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutputMatch {
    /// Connector name, e.g. `HDMI-A-1`. The default and most direct way to match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// EDID manufacturer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    /// EDID model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// EDID serial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl OutputMatch {
    pub fn by_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.make.is_none() && self.model.is_none() && self.serial.is_none()
    }

    /// Whether this rule selects `output`. Every specified field must match.
    pub fn matches(&self, output: &Output) -> bool {
        if self.is_empty() {
            return false;
        }
        let eq = |rule: &Option<String>, actual: &Option<String>| match rule {
            None => true,
            Some(want) => actual.as_deref().is_some_and(|have| have == want),
        };
        self.name.as_ref().is_none_or(|want| output.name == *want)
            && eq(&self.make, &output.make)
            && eq(&self.model, &output.model)
            && eq(&self.serial, &output.serial)
    }

    /// Stable key used to address this entry in the API and to detect duplicates.
    pub fn key(&self) -> String {
        if let Some(name) = &self.name {
            if self.make.is_none() && self.model.is_none() && self.serial.is_none() {
                return name.clone();
            }
        }
        let part = |value: &Option<String>| value.clone().unwrap_or_default();
        format!(
            "edid:{}:{}:{}:{}",
            part(&self.name),
            part(&self.make),
            part(&self.model),
            part(&self.serial)
        )
    }

    /// Inverse of [`OutputMatch::key`].
    pub fn parse_key(key: &str) -> Self {
        if let Some(rest) = key.strip_prefix("edid:") {
            let parts: Vec<&str> = rest.splitn(4, ':').collect();
            let get = |i: usize| {
                parts
                    .get(i)
                    .filter(|v| !v.is_empty())
                    .map(|v| (*v).to_string())
            };
            Self {
                name: get(0),
                make: get(1),
                model: get(2),
                serial: get(3),
            }
        } else {
            Self::by_name(key)
        }
    }
}

/// Screen rotation and flipping, as accepted by `sway-output(5)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
pub enum Transform {
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    #[serde(rename = "flipped")]
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

impl Transform {
    pub fn as_sway(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Rotate90 => "90",
            Self::Rotate180 => "180",
            Self::Rotate270 => "270",
            Self::Flipped => "flipped",
            Self::Flipped90 => "flipped-90",
            Self::Flipped180 => "flipped-180",
            Self::Flipped270 => "flipped-270",
        }
    }
}

/// Desired configuration for one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutputConfig {
    /// Which physical output this entry applies to.
    pub r#match: OutputMatch,
    /// Whether the output should be enabled. `false` actively disables it.
    #[serde(default = "default_true")]
    pub enable: bool,
    /// Mode to apply. When absent, Sway's preferred mode is left in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Position in the global layout. Suede performs no layout arithmetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    #[serde(default)]
    pub adaptive_sync: bool,
    /// Applied only when the detected Sway version supports it (≥ 1.10).
    #[serde(default)]
    pub allow_tearing: bool,
    /// Maximum milliseconds allowed to render a frame; `null` means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_render_time_ms: Option<u32>,
    /// What this output shows when no window covers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Background>,
}

/// How a wallpaper is scaled onto an output, matching `sway-output(5)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundMode {
    /// Scale to cover, cropping the overflow. The usual choice for signage.
    #[default]
    Fill,
    /// Scale to fit entirely, letterboxing the remainder.
    Fit,
    /// Scale to the output exactly, ignoring aspect ratio.
    Stretch,
    /// Original size, centred.
    Center,
    /// Original size, repeated.
    Tile,
}

impl BackgroundMode {
    pub fn as_sway(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Fit => "fit",
            Self::Stretch => "stretch",
            Self::Center => "center",
            Self::Tile => "tile",
        }
    }
}

/// What an output shows behind, or instead of, any window.
///
/// An appliance with a blank screen looks broken even when it is merely
/// between launches, so a background gives it something deliberate to show
/// while a browser restarts or before the first app starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Background {
    /// Id of an uploaded wallpaper. Absent means use `color` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallpaper: Option<String>,
    /// `#rrggbb`, shown where the wallpaper does not reach, or on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub mode: BackgroundMode,
}

impl Background {
    /// Whether this asks for anything at all.
    pub fn is_empty(&self) -> bool {
        self.wallpaper.is_none() && self.color.is_none()
    }

    /// Sway wants `#rrggbb` without the hash.
    pub fn sway_color(&self) -> Option<String> {
        self.color
            .as_ref()
            .map(|value| value.trim_start_matches('#').to_string())
    }
}

/// Wait for a URL to answer before launching an application.
///
/// A kiosk browser started before the service it points at is serving shows an
/// error page and stays there, since nothing reloads it. Gating the launch on
/// the service answering removes that race entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessConfig {
    /// URL to poll. Only `http://` is supported.
    pub url: String,
    /// Status codes that mean ready. Empty means any 2xx.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_status: Vec<u16>,
    /// How long between attempts.
    #[serde(default = "default_readiness_interval")]
    pub interval_seconds: u64,
    /// How long a single attempt may take.
    #[serde(default = "default_readiness_timeout")]
    pub timeout_seconds: u64,
    /// Give up waiting after this long and launch anyway. `null` waits forever,
    /// which is usually right for an appliance: showing an error page is worse
    /// than showing the background until the service appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub give_up_after_seconds: Option<u64>,
}

impl ReadinessConfig {
    /// Whether a status code counts as ready.
    pub fn accepts(&self, status: u16) -> bool {
        if self.expect_status.is_empty() {
            (200..300).contains(&status)
        } else {
            self.expect_status.contains(&status)
        }
    }
}

fn default_readiness_interval() -> u64 {
    2
}

fn default_readiness_timeout() -> u64 {
    5
}

impl OutputConfig {
    pub fn new(r#match: OutputMatch) -> Self {
        Self {
            r#match,
            enable: true,
            mode: None,
            position: None,
            scale: None,
            transform: None,
            adaptive_sync: false,
            allow_tearing: false,
            max_render_time_ms: None,
            background: None,
        }
    }
}

/// How an application is launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Launcher {
    /// Chromium with Suede's kiosk argument set.
    #[serde(rename_all = "camelCase")]
    ChromiumKiosk {
        /// URI to load. Supports `{appId}` and `{heartbeatUrl}` placeholders.
        uri: String,
        #[serde(default)]
        show_fps_counter: bool,
        /// Appended after the preset's arguments.
        #[serde(default)]
        extra_args: Vec<String>,
    },
    /// Firefox with its kiosk argument set.
    #[serde(rename_all = "camelCase")]
    FirefoxKiosk {
        uri: String,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    /// Any executable, launched verbatim.
    #[serde(rename_all = "camelCase")]
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl Launcher {
    /// True for launchers that need a private browser profile directory.
    pub fn is_chromium(&self) -> bool {
        matches!(self, Self::ChromiumKiosk { .. })
    }

    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::ChromiumKiosk { uri, .. } | Self::FirefoxKiosk { uri, .. } => Some(uri),
            Self::Exec { .. } => None,
        }
    }

    /// Executable this launcher needs on `PATH`.
    pub fn program(&self) -> &str {
        match self {
            Self::ChromiumKiosk { .. } => "chromium",
            Self::FirefoxKiosk { .. } => "firefox",
            Self::Exec { command, .. } => command,
        }
    }
}

/// Restart behaviour after an application exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicyKind {
    /// Always relaunch, whatever the exit status.
    #[default]
    Always,
    /// Relaunch only on a non-zero exit.
    OnFailure,
    /// Never relaunch automatically.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestartPolicy {
    pub policy: RestartPolicyKind,
    /// Initial delay before relaunching.
    pub delay_ms: u64,
    /// Ceiling for the exponential backoff.
    pub max_delay_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            policy: RestartPolicyKind::Always,
            delay_ms: 1000,
            max_delay_ms: 30_000,
        }
    }
}

impl RestartPolicy {
    /// Delay before attempt number `attempt` (1-based), with exponential backoff.
    pub fn delay_for(&self, attempt: u32) -> std::time::Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let scaled = self.delay_ms.saturating_mul(1u64 << shift);
        std::time::Duration::from_millis(scaled.min(self.max_delay_ms.max(self.delay_ms)))
    }

    pub fn should_restart(&self, exit_code: Option<i32>) -> bool {
        match self.policy {
            RestartPolicyKind::Always => true,
            RestartPolicyKind::OnFailure => exit_code != Some(0),
            RestartPolicyKind::Never => false,
        }
    }
}

/// Where an application's audio should go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioConfig {
    /// PipeWire `node.name` of the target sink. `null` routes to silence.
    pub output: Option<String>,
}

/// Content-level watchdog settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatConfig {
    pub enabled: bool,
    /// Silence tolerated once armed, before the app is killed and relaunched.
    #[serde(default = "default_heartbeat_timeout")]
    pub timeout_seconds: u64,
    /// Time allowed after launch for the first heartbeat to arrive.
    #[serde(default = "default_heartbeat_grace")]
    pub startup_grace_seconds: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_seconds: default_heartbeat_timeout(),
            startup_grace_seconds: default_heartbeat_grace(),
        }
    }
}

/// A managed application: a launch specification, not a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// Client-chosen, unique, stable identifier.
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub launcher: Launcher,
    /// Output the window is moved to. When absent, Sway's default placement applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputMatch>,
    #[serde(default = "default_true")]
    pub fullscreen: bool,
    /// Stretch one window across *every* output instead of filling one.
    ///
    /// This is sway's `fullscreen enable global`, and it is how a single
    /// browser drives a video wall: the outputs are positioned to form one
    /// canvas, and the window covers the whole of it. When set, `output`
    /// only decides where the window first appears.
    #[serde(default)]
    pub span_outputs: bool,
    /// Audio routing. Absent leaves routing untouched; `{"output": null}` silences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioConfig>,
    /// Extra environment variables for the launched process.
    ///
    /// Applied last, so they override anything the launcher preset sets.
    /// Hardware acceleration usually needs this: enabling NVDEC on an Nvidia
    /// card, for instance, is a matter of `LIBVA_DRIVER_NAME` and
    /// `NVD_BACKEND` rather than any command-line flag.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Wait for this URL to answer before launching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ReadinessConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<HeartbeatConfig>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Keep the browser profile between launches instead of wiping it.
    #[serde(default)]
    pub persist_profile: bool,
}

impl AppConfig {
    pub fn watchdog(&self) -> Option<&HeartbeatConfig> {
        self.heartbeat.as_ref().filter(|h| h.enabled)
    }

    /// Whether this app is expected to map a window.
    ///
    /// Browser presets always are. A bare `exec` might be a headless helper, so
    /// it only counts when an output is pinned — placement is the thing that
    /// needs a window.
    pub fn expects_window(&self) -> bool {
        match self.launcher {
            Launcher::ChromiumKiosk { .. } | Launcher::FirefoxKiosk { .. } => true,
            Launcher::Exec { .. } => self.output.is_some(),
        }
    }
}

/// Daemon-level settings that belong to desired state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Hide the pointer and park it beyond the layout.
    pub hide_cursor: bool,
    /// Backstop poll interval for output changes.
    pub output_poll_interval_seconds: u64,
    /// Enable the raw `POST /sway/command` passthrough.
    pub allow_raw_sway_commands: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hide_cursor: true,
            output_poll_interval_seconds: 5,
            allow_raw_sway_commands: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_heartbeat_timeout() -> u64 {
    25
}

fn default_heartbeat_grace() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::observed::Rect;

    fn output(name: &str, make: Option<&str>) -> Output {
        Output {
            name: name.into(),
            active: true,
            make: make.map(str::to_string),
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: Rect::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    #[test]
    fn match_by_name() {
        let rule = OutputMatch::by_name("HDMI-A-1");
        assert!(rule.matches(&output("HDMI-A-1", None)));
        assert!(!rule.matches(&output("HDMI-A-2", None)));
    }

    #[test]
    fn match_by_edid_requires_all_specified_fields() {
        let rule = OutputMatch {
            make: Some("Acme".into()),
            ..Default::default()
        };
        assert!(rule.matches(&output("HDMI-A-1", Some("Acme"))));
        assert!(!rule.matches(&output("HDMI-A-1", Some("Other"))));
        assert!(!rule.matches(&output("HDMI-A-1", None)));
    }

    #[test]
    fn empty_match_never_matches() {
        assert!(!OutputMatch::default().matches(&output("HDMI-A-1", None)));
    }

    #[test]
    fn key_round_trips() {
        for rule in [
            OutputMatch::by_name("HDMI-A-1"),
            OutputMatch {
                make: Some("Acme".into()),
                model: Some("X1".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(OutputMatch::parse_key(&rule.key()), rule);
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let policy = RestartPolicy {
            policy: RestartPolicyKind::Always,
            delay_ms: 1000,
            max_delay_ms: 30_000,
        };
        assert_eq!(policy.delay_for(1).as_millis(), 1000);
        assert_eq!(policy.delay_for(2).as_millis(), 2000);
        assert_eq!(policy.delay_for(3).as_millis(), 4000);
        assert_eq!(policy.delay_for(20).as_millis(), 30_000);
    }

    #[test]
    fn on_failure_ignores_clean_exit() {
        let policy = RestartPolicy {
            policy: RestartPolicyKind::OnFailure,
            ..Default::default()
        };
        assert!(!policy.should_restart(Some(0)));
        assert!(policy.should_restart(Some(1)));
        assert!(policy.should_restart(None));
    }

    #[test]
    fn validation_catches_duplicate_app_ids() {
        let app = AppConfig {
            id: "a".into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "true".into(),
                args: vec![],
            },
            output: None,
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        };
        let state = DesiredState {
            apps: vec![app.clone(), app],
            ..DesiredState::new()
        };
        let errors = state.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("is not unique")));
    }

    #[test]
    fn validation_rejects_bad_environment_names() {
        let mut app = AppConfig {
            id: "a".into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "true".into(),
                args: vec![],
            },
            output: None,
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        };
        app.env.insert("BAD=NAME".into(), "x".into());
        let state = DesiredState {
            apps: vec![app],
            ..DesiredState::new()
        };
        let errors = state.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("invalid variable name")));
    }

    #[test]
    fn validation_rejects_unsafe_app_id() {
        let state = DesiredState {
            apps: vec![AppConfig {
                id: "../escape".into(),
                enabled: true,
                launcher: Launcher::Exec {
                    command: "true".into(),
                    args: vec![],
                },
                output: None,
                fullscreen: true,
                span_outputs: false,
                env: Default::default(),
                readiness: None,
                audio: None,
                heartbeat: None,
                restart: RestartPolicy::default(),
                persist_profile: false,
            }],
            ..DesiredState::new()
        };
        assert!(state.validate().is_err());
    }

    #[test]
    fn launcher_round_trips_through_json() {
        let launcher = Launcher::ChromiumKiosk {
            uri: "http://example.com".into(),
            show_fps_counter: true,
            extra_args: vec!["--mute-audio".into()],
        };
        let json = serde_json::to_value(&launcher).unwrap();
        assert_eq!(json["kind"], "chromium-kiosk");
        assert_eq!(json["showFpsCounter"], true);
        let back: Launcher = serde_json::from_value(json).unwrap();
        assert_eq!(back, launcher);
    }

    #[test]
    fn audio_absent_and_null_are_distinguishable() {
        let absent: AppConfig =
            serde_json::from_str(r#"{"id":"a","launcher":{"kind":"exec","command":"true"}}"#)
                .unwrap();
        assert!(absent.audio.is_none());
        let silent: AppConfig = serde_json::from_str(
            r#"{"id":"a","launcher":{"kind":"exec","command":"true"},"audio":{"output":null}}"#,
        )
        .unwrap();
        assert_eq!(silent.audio, Some(AudioConfig { output: None }));
    }
}
