//! Bootstrap configuration: everything that must be known before the API can serve.
//!
//! Read once at startup from `$XDG_CONFIG_HOME/suede/suede.toml`; every value
//! but [`BootstrapConfig::allow_overlaps`] and
//! [`BootstrapConfig::direct_scanout`] is overridable by a `SUEDE_*`
//! environment variable, which wins. Those two describe how the compositor
//! was started, which an environment variable on *Suede's* process cannot
//! change. Everything else is desired state, owned by the API (see
//! [`crate::state`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::PowerVerb;
use crate::supervisor::launcher::{CHROMIUM_PROGRAMS, FIREFOX_PROGRAMS};

pub const DEFAULT_BIND: &str = "0.0.0.0:9088";
pub const DEFAULT_DOCS_BASE_URL: &str = "https://suede.gameshow.pro/";

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bind: Option<String>,
    token: Option<String>,
    state_dir: Option<PathBuf>,
    docs_base_url: Option<String>,
    /// Raw strings rather than `Vec<PowerVerb>`: a bad entry needs to become a
    /// [`ConfigError::PowerVerb`] naming the value and the accepted ones, not
    /// whatever wording `toml`'s own enum deserialisation happens to produce,
    /// and the same parse has to serve the `SUEDE_POWER` override too.
    power: Option<Vec<String>>,
    /// `Option`, not a bare `Vec`, so a present-but-empty list (permit
    /// nothing) can be told apart from an absent key (use the browser
    /// default) — `serde`'s usual "missing means empty" would conflate the
    /// two, and they mean opposite things.
    allowed_programs: Option<Vec<String>>,
    allow_overlaps: Option<bool>,
    /// `Option`, not a bare `bool`, so an *explicit* `direct_scanout = true`
    /// can be told apart from an absent key. They resolve to the same value
    /// — true is the default — but only the explicit one is refused when
    /// `allow_overlaps` is false, where scanning out is not safe; an absent
    /// key must never turn an upgrade into a startup failure.
    direct_scanout: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Address the HTTP server binds to.
    pub bind: SocketAddr,
    /// Optional static bearer token. When set, the reference web UI is disabled.
    pub token: Option<String>,
    /// Directory holding the persisted desired state.
    pub state_dir: PathBuf,
    /// Base URL used to build `docsUrl` links in health checks.
    pub docs_base_url: String,
    /// Sway configuration file that health-check fixes may patch.
    ///
    /// Held here rather than derived at the point of use so the remediations
    /// can be exercised against a temporary directory instead of a real home.
    pub sway_config_path: PathBuf,
    /// Directory for systemd *user* units that fixes may write.
    pub systemd_user_dir: PathBuf,
    /// Host power operations this appliance permits, e.g. `["reboot"]`.
    ///
    /// Empty by default: a display appliance should not be able to turn itself
    /// off because somebody found the button. This lives here, in the file, and
    /// not in desired state, because desired state is writable through the very
    /// API the permission is meant to constrain — a gate the caller can open is
    /// not a gate.
    pub power: Vec<PowerVerb>,
    /// Programs applications are permitted to launch.
    ///
    /// Defaults to the browsers Suede knows how to drive, so a stock appliance
    /// runs kiosk pages and nothing else. `["*"]` lifts the restriction
    /// entirely; an empty list permits nothing, which is what an empty allowlist
    /// means everywhere else and is a legitimate way to freeze a machine.
    ///
    /// In the file rather than in desired state, because desired state is
    /// written through the API this is meant to constrain.
    pub allowed_programs: Vec<String>,
    /// Whether outputs may overlap in canvas space — the one setting that
    /// decides which of the two display paths this machine runs.
    ///
    /// `false` (the default, and what `provision.sh` provisions): sway tiles
    /// the layout itself and a spanning application is one window across all
    /// of it, so a write whose output rectangles intersect is rejected rather
    /// than quietly producing a layout sway cannot render distinctly. The
    /// compositor must then run with `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1`,
    /// or spanning mirrors on some drivers.
    ///
    /// `true`: every layout of two or more outputs goes through the slicer,
    /// overlapping or not — each display shows one private, output-sized
    /// buffer, which is the case direct scanout was built for, so the
    /// compositor should run *without* that variable.
    ///
    /// Bootstrap rather than desired state because it is a fact about how the
    /// machine's compositor was started, which the API cannot change: a client
    /// writing it would only claim an environment that is already fixed.
    pub allow_overlaps: bool,
    /// Whether the compositor may flip the slicer's buffers straight to the
    /// display controllers — the A/B half of [`Self::allow_overlaps`].
    ///
    /// Defaults to `true`, and only means anything while `allow_overlaps` is
    /// true: that is the layout where no client spans the physical outputs,
    /// so scanning out cannot mirror. Setting it `false` there asks the
    /// compositor to composite each slice instead, which is the arm to
    /// compare against when measuring what scanout actually buys — flip the
    /// key, restart the compositor, and read `zeroCopyPresented`, straddles
    /// and `lagFrames` from `GET /projection/stats`.
    ///
    /// An *explicit* `direct_scanout = true` alongside `allow_overlaps =
    /// false` is refused at startup (see
    /// [`ConfigError::DirectScanoutWithoutOverlaps`]): there one window spans
    /// every output, and scanning that out mirrors instead of spanning on
    /// some drivers. The key being absent is not refused, because true is
    /// simply its default.
    pub direct_scanout: bool,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            token: None,
            state_dir: crate::util::state_dir(),
            docs_base_url: DEFAULT_DOCS_BASE_URL.to_string(),
            sway_config_path: default_sway_config_path(),
            systemd_user_dir: default_systemd_user_dir(),
            power: Vec::new(),
            allowed_programs: default_allowed_programs(),
            allow_overlaps: false,
            direct_scanout: true,
        }
    }
}

/// The browsers Suede knows how to drive, taken from the launcher presets
/// themselves rather than retyped here — so this default and the presets'
/// own search lists cannot drift apart.
fn default_allowed_programs() -> Vec<String> {
    CHROMIUM_PROGRAMS
        .iter()
        .chain(FIREFOX_PROGRAMS.iter())
        .map(|program| program.to_string())
        .collect()
}

fn config_home() -> PathBuf {
    crate::util::config_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_sway_config_path() -> PathBuf {
    config_home().join("sway/config")
}

fn default_systemd_user_dir() -> PathBuf {
    config_home().join("systemd/user")
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid bind address {value:?}: {source}")]
    Bind {
        value: String,
        #[source]
        source: std::net::AddrParseError,
    },
    /// A typo here must not silently disable the feature: better a daemon
    /// that refuses to start than one that quietly grants no power at all
    /// while the operator believes `power = ["reboot"]` took effect.
    #[error(
        "unknown power verb {value:?}; accepted values are {}",
        PowerVerb::ALL.iter().map(PowerVerb::as_str).collect::<Vec<_>>().join(", ")
    )]
    PowerVerb { value: String },
    /// Only the slicer's layout can be scanned out safely, so asking for
    /// scanout on a tiling appliance is a request that cannot be honoured —
    /// and honouring it anyway would re-open the mirroring bug on the very
    /// machines the default protects. Refused rather than ignored: an
    /// operator who wrote the key meant something by it.
    #[error(
        "direct_scanout = true needs allow_overlaps = true, but allow_overlaps is false. \
         With allow_overlaps = false sway tiles the layout and one window spans every \
         output; scanning that window out makes some drivers show the same part of it \
         on each display. Set allow_overlaps = true to project through the slicer, or \
         remove direct_scanout — it only has an effect there."
    )]
    DirectScanoutWithoutOverlaps,
}

impl BootstrapConfig {
    /// Default path of the bootstrap config file.
    pub fn default_path() -> PathBuf {
        crate::util::config_dir().join("suede.toml")
    }

    /// Load from `path` (missing file means all defaults), then apply `SUEDE_*` overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str::<FileConfig>(&text).map_err(|source| ConfigError::Parse {
                    path: path.clone(),
                    source,
                })?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no bootstrap config file; using defaults");
                FileConfig::default()
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };

        let mut config = Self::default();

        if let Some(bind) = env_or(file.bind, "SUEDE_BIND") {
            config.bind = bind.parse().map_err(|source| ConfigError::Bind {
                value: bind.clone(),
                source,
            })?;
        }
        config.token = env_or(file.token, "SUEDE_TOKEN").filter(|t| !t.is_empty());
        if let Some(dir) = std::env::var("SUEDE_STATE_DIR")
            .ok()
            .filter(|v| !v.is_empty())
        {
            config.state_dir = PathBuf::from(dir);
        } else if let Some(dir) = file.state_dir {
            config.state_dir = dir;
        }
        if let Some(url) = env_or(file.docs_base_url, "SUEDE_DOCS_BASE_URL") {
            config.docs_base_url = url;
        }
        config.power = parse_power(power_source(file.power))?;
        config.allowed_programs =
            allowed_programs_source(file.allowed_programs).unwrap_or_else(default_allowed_programs);
        config.allow_overlaps = file.allow_overlaps.unwrap_or(false);
        // An absent key resolves to the same `true` as an explicit one; only
        // the explicit one is an error on a tiling appliance, because only it
        // says the operator wanted something the machine cannot do.
        if file.direct_scanout == Some(true) && !config.allow_overlaps {
            return Err(ConfigError::DirectScanoutWithoutOverlaps);
        }
        config.direct_scanout = file.direct_scanout.unwrap_or(true);

        Ok(config)
    }

    /// True when a bearer token is configured, which also disables the web UI.
    pub fn auth_enabled(&self) -> bool {
        self.token.is_some()
    }

    /// Whether the compositor is expected to run *with* direct scanout — that
    /// is, started without `WLR_SCENE_DISABLE_DIRECT_SCANOUT`.
    ///
    /// The one place the two keys are combined, so the health check, the
    /// startup log and `provision.sh`'s login-time derivation cannot reach
    /// different conclusions about the same file.
    pub fn scanout_expected(&self) -> bool {
        self.allow_overlaps && self.direct_scanout
    }

    /// Whether this appliance is permitted to perform `verb`.
    pub fn power_allows(&self, verb: PowerVerb) -> bool {
        self.power.contains(&verb)
    }

    /// Whether `program` may be launched. Matching is on the file name, so
    /// `/usr/local/bin/chromium` and `chromium` are the same program; a list
    /// containing `*` allows everything.
    pub fn program_allowed(&self, program: &Path) -> bool {
        program_name_matches(&self.allowed_programs, program)
    }

    /// Build a documentation URL for a health check.
    pub fn docs_url(&self, relative: &str) -> String {
        format!(
            "{}{}",
            self.docs_base_url.trim_end_matches('/'),
            if relative.starts_with('/') {
                relative.to_string()
            } else {
                format!("/{relative}")
            }
        )
    }

    /// Warn loudly about an unauthenticated, non-loopback deployment.
    pub fn log_security_posture(&self) {
        if self.auth_enabled() {
            tracing::info!("bearer token configured; reference web UI is disabled");
        } else if !self.bind.ip().is_loopback() {
            tracing::warn!(
                bind = %self.bind,
                "serving without authentication on a non-loopback address; \
                 set SUEDE_TOKEN if this network is not trusted"
            );
        }
        // Worth its own line, unconditionally: this is the one bootstrap
        // setting that lets a request turn the machine off, and that should
        // be visible in the journal of a box that did, without having to go
        // looking for it.
        if !self.power.is_empty() {
            tracing::info!(verbs = ?self.power, "host power control is enabled");
        }
    }
}

fn env_or(file_value: Option<String>, var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .or(file_value)
}

/// Raw power-verb spellings from `SUEDE_POWER` (comma-separated, wins
/// outright) or the file list — same all-or-nothing precedence as every
/// other `env_or` field, just shaped for a list instead of a scalar.
fn power_source(file_value: Option<Vec<String>>) -> Vec<String> {
    match std::env::var("SUEDE_POWER").ok().filter(|v| !v.is_empty()) {
        Some(value) => value
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect(),
        None => file_value.unwrap_or_default(),
    }
}

/// Raw program names from `SUEDE_ALLOWED_PROGRAMS` (comma-separated, wins
/// outright) or the file list.
///
/// Unlike [`power_source`], `None` is a meaningful outcome here, not just an
/// empty `Vec`: the file's `Option` is threaded straight through so `load`
/// can tell "the key was absent, use the browser default" apart from "the
/// key was `[]`, permit nothing" — collapsing them with `unwrap_or_default`
/// would make an explicit empty list indistinguishable from no opinion at
/// all, and they mean opposite things.
fn allowed_programs_source(file_value: Option<Vec<String>>) -> Option<Vec<String>> {
    match std::env::var("SUEDE_ALLOWED_PROGRAMS")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(value) => Some(
            value
                .split(',')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect(),
        ),
        None => file_value,
    }
}

/// The matching rule behind [`BootstrapConfig::program_allowed`], usable
/// wherever only the raw list is at hand — the supervisor keeps a copy of
/// the list rather than the whole bootstrap config, since launching is all
/// it needs from it, and this is the one place the comparison is written so
/// the two callers cannot disagree about what "allowed" means.
pub(crate) fn program_name_matches(allowed: &[String], program: &Path) -> bool {
    if allowed.iter().any(|p| p == "*") {
        return true;
    }
    let Some(name) = program.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    allowed.iter().any(|p| p == name)
}

/// Parse and deduplicate, preserving first-seen order — the order a client
/// might reasonably expect back from `GET /system`.
fn parse_power(values: Vec<String>) -> Result<Vec<PowerVerb>, ConfigError> {
    let mut verbs = Vec::new();
    for value in values {
        let verb = PowerVerb::parse(&value).ok_or_else(|| ConfigError::PowerVerb {
            value: value.clone(),
        })?;
        if !verbs.contains(&verb) {
            verbs.push(verb);
        }
    }
    Ok(verbs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that reads or writes a `SUEDE_*` variable.
    ///
    /// `BootstrapConfig::load` reads *all* of them, so it is not enough for
    /// each test to own the one variable it sets. The power test deliberately
    /// sets `SUEDE_POWER` to a bad value to prove a typo fails loudly, and
    /// while that stands, any other test calling `load` gets that error
    /// instead of its own answer. That is precisely what happened: two tests
    /// that each passed alone failed together, and only in a full run.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the environment lock, ignoring poisoning: a panicking test has
    /// already failed the run, and turning its neighbours into confusing
    /// secondary failures helps nobody find it.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn missing_file_yields_defaults() {
        let _guard = env_lock();
        let config = BootstrapConfig::load(Some(Path::new("/nonexistent/suede.toml"))).unwrap();
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert!(config.token.is_none());
    }

    #[test]
    fn docs_url_joins_without_double_slash() {
        let config = BootstrapConfig {
            docs_base_url: "https://example.com/".into(),
            ..Default::default()
        };
        assert_eq!(
            config.docs_url("troubleshooting/#sway"),
            "https://example.com/troubleshooting/#sway"
        );
    }

    #[test]
    fn file_values_are_read() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suede.toml");
        std::fs::write(&path, "bind = \"127.0.0.1:9000\"\ntoken = \"abc\"\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert_eq!(config.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(config.token.as_deref(), Some("abc"));
        assert!(config.auth_enabled());
    }

    /// One test, not several, because `SUEDE_POWER` is process-wide state and
    /// parallel test binaries would otherwise race each other reading and
    /// setting it — the same reasoning `watchdog::tests` uses for `WATCHDOG_USEC`.
    #[test]
    fn power_verbs_are_parsed_from_file_and_env() {
        let _guard = env_lock();
        // Safety: the whole test holds `env_lock`, so no other test in this
        // binary is reading or writing the environment meanwhile.
        unsafe { std::env::remove_var("SUEDE_POWER") };

        // Unset and empty both mean none.
        let config = BootstrapConfig::load(Some(Path::new("/nonexistent/suede.toml"))).unwrap();
        assert!(config.power.is_empty());
        assert!(!config.power_allows(PowerVerb::Reboot));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suede.toml");

        // A TOML list, deduplicated, in the order it was written.
        std::fs::write(&path, "power = [\"reboot\", \"poweroff\", \"reboot\"]\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert_eq!(config.power, vec![PowerVerb::Reboot, PowerVerb::Poweroff]);
        assert!(config.power_allows(PowerVerb::Reboot));
        assert!(config.power_allows(PowerVerb::Poweroff));

        // SUEDE_POWER wins outright over the file, exactly like every other
        // bootstrap value's environment override.
        unsafe { std::env::set_var("SUEDE_POWER", "poweroff") };
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert_eq!(config.power, vec![PowerVerb::Poweroff]);
        assert!(!config.power_allows(PowerVerb::Reboot));

        // A typo must fail loudly, naming the value and the accepted ones —
        // silently disabling the feature would be the worse failure mode.
        unsafe { std::env::set_var("SUEDE_POWER", "shutdown") };
        let error = BootstrapConfig::load(Some(&path)).unwrap_err().to_string();
        assert!(error.contains("shutdown"), "{error}");
        assert!(error.contains("reboot"), "{error}");
        assert!(error.contains("poweroff"), "{error}");

        unsafe { std::env::remove_var("SUEDE_POWER") };
    }

    #[test]
    fn allow_overlaps_defaults_to_off_and_is_read_from_the_file() {
        let _guard = env_lock();
        // Absent means tiling: the path every existing appliance already
        // runs, so an upgrade cannot change what a machine does at its next
        // boot.
        let config = BootstrapConfig::load(Some(Path::new("/nonexistent/suede.toml"))).unwrap();
        assert!(!config.allow_overlaps);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suede.toml");

        std::fs::write(&path, "allow_overlaps = true\n").unwrap();
        assert!(BootstrapConfig::load(Some(&path)).unwrap().allow_overlaps);

        std::fs::write(&path, "allow_overlaps = false\n").unwrap();
        assert!(!BootstrapConfig::load(Some(&path)).unwrap().allow_overlaps);

        // A misspelling is refused outright rather than silently leaving the
        // machine on the other display path.
        std::fs::write(&path, "allow_overlap = true\n").unwrap();
        assert!(BootstrapConfig::load(Some(&path)).is_err());
    }

    #[test]
    fn direct_scanout_defaults_to_on_and_only_means_anything_with_overlaps() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suede.toml");

        // Absent: true, and on a tiling appliance that is not an error — an
        // upgrade must not start failing on a file nobody edited.
        let config = BootstrapConfig::load(Some(Path::new("/nonexistent/suede.toml"))).unwrap();
        assert!(config.direct_scanout);
        assert!(!config.scanout_expected());

        std::fs::write(&path, "allow_overlaps = true\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert!(config.direct_scanout);
        assert!(config.scanout_expected());

        // The A/B's other arm: slice every layout, but have the compositor
        // composite the slices instead of flipping them.
        std::fs::write(&path, "allow_overlaps = true\ndirect_scanout = false\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert!(!config.direct_scanout);
        assert!(!config.scanout_expected());

        // Explicitly on, with the slicer: allowed, and the same as absent.
        std::fs::write(&path, "allow_overlaps = true\ndirect_scanout = true\n").unwrap();
        assert!(BootstrapConfig::load(Some(&path))
            .unwrap()
            .scanout_expected());

        // Explicitly on while sway tiles: refused, naming both keys, because
        // the spanning window it would scan out is the mirroring bug.
        std::fs::write(&path, "direct_scanout = true\n").unwrap();
        let error = BootstrapConfig::load(Some(&path)).unwrap_err().to_string();
        assert!(error.contains("direct_scanout = true"), "{error}");
        assert!(error.contains("allow_overlaps"), "{error}");

        // Off while sway tiles is merely redundant, not wrong.
        std::fs::write(&path, "allow_overlaps = false\ndirect_scanout = false\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert!(!config.scanout_expected());

        std::fs::write(&path, "direct_scanouts = false\n").unwrap();
        assert!(BootstrapConfig::load(Some(&path)).is_err());
    }

    #[test]
    fn program_allowed_matches_by_file_name() {
        let config = BootstrapConfig {
            allowed_programs: vec!["chromium".to_string()],
            ..Default::default()
        };
        assert!(config.program_allowed(Path::new("chromium")));
        assert!(config.program_allowed(Path::new("/usr/bin/chromium")));
        assert!(!config.program_allowed(Path::new("bash")));
        assert!(!config.program_allowed(Path::new("/usr/bin/bash")));
    }

    #[test]
    fn a_star_entry_allows_everything() {
        let config = BootstrapConfig {
            allowed_programs: vec!["*".to_string()],
            ..Default::default()
        };
        assert!(config.program_allowed(Path::new("bash")));
        assert!(config.program_allowed(Path::new("/anything/at/all")));
    }

    #[test]
    fn an_empty_list_permits_nothing() {
        let config = BootstrapConfig {
            allowed_programs: Vec::new(),
            ..Default::default()
        };
        assert!(!config.program_allowed(Path::new("chromium")));
    }

    #[test]
    fn the_default_list_accepts_chromium_and_refuses_bash() {
        let config = BootstrapConfig::default();
        assert!(config.program_allowed(Path::new("chromium")));
        assert!(!config.program_allowed(Path::new("bash")));
    }

    /// One test, not several, for the same reason as
    /// `power_verbs_are_parsed_from_file_and_env`: `SUEDE_ALLOWED_PROGRAMS`
    /// is process-wide state that parallel test binaries would otherwise
    /// race to set.
    #[test]
    fn allowed_programs_are_parsed_from_file_and_env() {
        let _guard = env_lock();
        // Safety: the whole test holds `env_lock`, so no other test in this
        // binary is reading or writing the environment meanwhile.
        unsafe { std::env::remove_var("SUEDE_ALLOWED_PROGRAMS") };

        // Absent key: the browser default, not an empty list.
        let config = BootstrapConfig::load(Some(Path::new("/nonexistent/suede.toml"))).unwrap();
        assert_eq!(config.allowed_programs, default_allowed_programs());
        assert!(config.program_allowed(Path::new("chromium")));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suede.toml");

        // A TOML list is read verbatim.
        std::fs::write(&path, "allowed_programs = [\"mpv\"]\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert_eq!(config.allowed_programs, vec!["mpv".to_string()]);

        // An explicitly empty list stays empty rather than falling back to
        // the default — this is how an operator freezes a machine.
        std::fs::write(&path, "allowed_programs = []\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert!(config.allowed_programs.is_empty());
        assert!(!config.program_allowed(Path::new("chromium")));

        // SUEDE_ALLOWED_PROGRAMS wins outright over the file, exactly like
        // every other bootstrap value's environment override.
        unsafe { std::env::set_var("SUEDE_ALLOWED_PROGRAMS", "firefox, mpv") };
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert_eq!(
            config.allowed_programs,
            vec!["firefox".to_string(), "mpv".to_string()]
        );

        unsafe { std::env::remove_var("SUEDE_ALLOWED_PROGRAMS") };
    }
}
