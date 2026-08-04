//! Environment health checks and their remediations.
//!
//! Suede verifies the environment it depends on rather than silently mutating
//! it. Failures carry a documentation link; only fixes within the session
//! user's power are offered, and none ever run implicitly.

pub mod config_block;

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use crate::audio::AudioMonitor;
use crate::config::BootstrapConfig;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventHub, ServerEvent};
use crate::model::{Check, CheckStatus, PackageVersion};
use crate::sway::SwayClient;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const UNIT_NAME: &str = "suede.service";

/// Checks that `fix()` can remediate. Kept beside the dispatch so the two
/// cannot drift apart.
pub const FIXABLE: &[&str] = &[
    ids::SYSTEMD_UNIT,
    ids::SWAY_CONFIG,
    ids::PIPEWIRE,
    ids::DIRECT_SCANOUT,
];

/// Check identifiers, also used as the `{id}` in the fix endpoint.
pub mod ids {
    pub const SWAY_SOCKET: &str = "sway-socket";
    pub const SWAY_VERSION: &str = "sway-version";
    pub const WAYLAND_DISPLAY: &str = "wayland-display";
    pub const DIRECT_SCANOUT: &str = "direct-scanout";
    pub const REAL_DISPLAYS: &str = "real-displays";
    pub const VIDEO_DECODE: &str = "video-decode";
    pub const SWAYBG: &str = "swaybg";
    pub const BROWSERS: &str = "browsers";
    pub const PIPEWIRE: &str = "pipewire";
    pub const SYSTEMD_UNIT: &str = "systemd-unit";
    pub const SWAY_CONFIG: &str = "sway-config";
    pub const STATE_DIR: &str = "state-dir";
    pub const API_REACHABILITY: &str = "api-reachability";
}

/// A host packet filter that may be dropping traffic to the API port.
///
/// Only the presence of one can be established without root: `ufw status` and
/// `nft list ruleset` both need privileges Suede deliberately does not have.
/// That is enough to be useful — the failure this catches looks identical to a
/// dead appliance, so naming the suspect is most of the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFirewall {
    Ufw,
    Firewalld,
    Nftables,
    None,
}

impl HostFirewall {
    /// The systemd unit whose being active implies this filter.
    fn unit(self) -> Option<&'static str> {
        Some(match self {
            Self::Ufw => "ufw",
            Self::Firewalld => "firewalld",
            Self::Nftables => "nftables",
            Self::None => return None,
        })
    }

    /// What the operator would run to open `port`, where there is a one-liner.
    fn allow_command(self, port: u16) -> Option<String> {
        Some(match self {
            Self::Ufw => format!("sudo ufw allow {port}/tcp"),
            Self::Firewalld => format!(
                "sudo firewall-cmd --permanent --add-port={port}/tcp && sudo firewall-cmd --reload"
            ),
            // nftables has no stable one-liner: it depends on the table and
            // chain names in use, so sending them to the docs is honest.
            Self::Nftables | Self::None => return None,
        })
    }
}

/// Decide what to say about who can reach the API.
///
/// Pure, so every combination is table-testable: the interesting cases involve
/// a firewall that cannot be inspected on the machine running the tests.
///
/// `last_remote` is the address of the most recent off-box client, if any.
/// It outranks everything inferred: a request that crossed the network is
/// proof the port is open, where a running firewall is only a suspicion.
fn assess_reachability(
    bind: std::net::SocketAddr,
    firewall: HostFirewall,
    authenticated: bool,
    last_remote: Option<std::net::IpAddr>,
) -> (CheckStatus, String) {
    if bind.ip().is_loopback() {
        return (
            CheckStatus::Pass,
            format!(
                "bound to {bind}, so the API answers only on this machine. \
                 Reach it from elsewhere with `ssh -L {port}:127.0.0.1:{port} <host>`, \
                 or bind a routable address to expose it.",
                port = bind.port()
            ),
        );
    }

    let exposure = if authenticated {
        "a bearer token is required"
    } else {
        "no token is set, so anyone who can reach it has full control"
    };

    if let Some(peer) = last_remote {
        return (
            CheckStatus::Pass,
            format!("bound to {bind} and confirmed reachable — {peer} has connected; {exposure}"),
        );
    }

    match firewall.unit() {
        None => (
            CheckStatus::Pass,
            format!(
                "bound to {bind} with no host firewall running, though nothing off this \
                 machine has connected yet; {exposure}"
            ),
        ),
        Some(unit) => {
            // The port cannot be tested from here: traffic from the appliance
            // to its own address never crosses the filter, so it would pass
            // whether or not anything else can connect. And the rules cannot
            // be read — they are root-only — so evidence is all there is.
            let remedy = match firewall.allow_command(bind.port()) {
                Some(command) => format!("Open it with: {command}"),
                None => "Open it in the ruleset for this host.".to_string(),
            };
            (
                CheckStatus::Warn,
                format!(
                    "bound to {bind}, but {unit} is running, its rules are root-only, and \
                     nothing off this machine has connected yet. If the appliance is \
                     unreachable, the port is being dropped — which looks exactly like a \
                     daemon that is not running. {remedy} This clears itself as soon as one \
                     remote client connects. ({exposure}.)"
                ),
            )
        }
    }
}

pub struct CheckRunner {
    bootstrap: Arc<BootstrapConfig>,
    sway: Arc<dyn SwayClient>,
    audio: Arc<dyn AudioMonitor>,
    store: Arc<crate::state::StateStore>,
    events: EventHub,
    results: RwLock<Vec<Check>>,
    /// Most recent client that was not on this machine. See [`note_client`].
    ///
    /// [`note_client`]: CheckRunner::note_client
    last_remote_client: RwLock<Option<std::net::IpAddr>>,
}

impl CheckRunner {
    pub fn new(
        bootstrap: Arc<BootstrapConfig>,
        sway: Arc<dyn SwayClient>,
        audio: Arc<dyn AudioMonitor>,
        store: Arc<crate::state::StateStore>,
        events: EventHub,
    ) -> Self {
        Self {
            bootstrap,
            sway,
            audio,
            store,
            events,
            results: RwLock::new(Vec::new()),
            last_remote_client: RwLock::new(None),
        }
    }

    pub fn results(&self) -> Vec<Check> {
        self.results.read().unwrap().clone()
    }

    /// Record that a request arrived from `peer`.
    ///
    /// Loopback callers are ignored: the daemon's own health probes and the
    /// browsers posting heartbeats would otherwise "prove" a reachability the
    /// network has never actually demonstrated.
    pub fn note_client(&self, peer: std::net::IpAddr) {
        if peer.is_loopback() {
            return;
        }
        let mut guard = self.last_remote_client.write().unwrap();
        if *guard != Some(peer) {
            *guard = Some(peer);
        }
    }

    /// The most recent off-box client, if one has ever connected.
    pub fn last_remote_client(&self) -> Option<std::net::IpAddr> {
        *self.last_remote_client.read().unwrap()
    }

    /// Run every check, publishing an event when the outcome changes.
    pub async fn run_all(&self) -> Vec<Check> {
        let checks = vec![
            self.check_sway_socket().await,
            self.check_sway_version().await,
            self.check_wayland_display(),
            self.check_direct_scanout(),
            self.check_real_displays().await,
            self.check_video_decode(),
            self.check_swaybg().await,
            self.check_browsers().await,
            self.check_pipewire().await,
            self.check_systemd_unit().await,
            self.check_sway_config(),
            self.check_state_dir(),
            self.check_api_reachability().await,
        ];

        let changed = {
            let mut guard = self.results.write().unwrap();
            let changed = *guard != checks;
            if changed {
                *guard = checks.clone();
            }
            changed
        };
        if changed {
            self.events
                .publish(ServerEvent::ChecksChanged(checks.clone()));
        }
        checks
    }

    /// Versions of the packages Suede depends on, for `GET /system`.
    pub async fn package_versions(&self) -> Vec<PackageVersion> {
        let mut packages = Vec::new();
        for (name, program, arg) in [
            ("sway", "sway", "--version"),
            ("chromium", "chromium", "--version"),
            ("firefox", "firefox", "--version"),
            ("pipewire", "pipewire", "--version"),
        ] {
            let version = run(program, &[arg])
                .await
                .ok()
                .filter(|output| output.success)
                .map(|output| first_line(&output.stdout));
            packages.push(PackageVersion {
                name: name.to_string(),
                version,
            });
        }
        packages
    }

    // --- individual checks ------------------------------------------------

    async fn check_sway_socket(&self) -> Check {
        let connected = self.sway.is_connected();
        let socket = crate::sway::discover_socket();
        let (status, detail) = match (connected, &socket) {
            (true, Some(path)) => (
                CheckStatus::Pass,
                format!("connected via {}", path.display()),
            ),
            (false, Some(path)) => (
                CheckStatus::Warn,
                format!(
                    "socket {} exists but the event connection is down",
                    path.display()
                ),
            ),
            (_, None) => (
                CheckStatus::Fail,
                "no sway IPC socket found; is sway running in this session?".to_string(),
            ),
        };
        self.check(ids::SWAY_SOCKET, "Sway IPC reachable", status, detail, None)
    }

    async fn check_sway_version(&self) -> Check {
        match self.sway.get_version().await {
            Ok(version) => {
                let detail = if version.supports_tearing() {
                    format!("{} (tearing control available)", version.display())
                } else {
                    format!(
                        "{} — tearing control needs sway 1.10 or newer",
                        version.display()
                    )
                };
                let status = if version.supports_tearing() {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Warn
                };
                self.check(ids::SWAY_VERSION, "Sway version", status, detail, None)
            }
            Err(error) => self.check(
                ids::SWAY_VERSION,
                "Sway version",
                CheckStatus::Fail,
                format!("could not query sway: {error}"),
                None,
            ),
        }
    }

    /// Applications inherit Suede's environment, so a missing `WAYLAND_DISPLAY`
    /// means every launch fails immediately — with an error that would
    /// otherwise only appear in the app's own output.
    fn check_wayland_display(&self) -> Check {
        let display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|v| !v.is_empty());
        let socket = display.as_ref().and_then(|name| {
            let path = std::path::Path::new(name);
            if path.is_absolute() {
                Some(path.to_path_buf())
            } else {
                crate::util::runtime_dir().map(|dir| dir.join(name))
            }
        });

        let (status, detail) = match (&display, &socket) {
            (Some(name), Some(path)) if path.exists() => (
                CheckStatus::Pass,
                format!("WAYLAND_DISPLAY={name} ({})", path.display()),
            ),
            (Some(name), Some(path)) => (
                CheckStatus::Fail,
                format!(
                    "WAYLAND_DISPLAY={name} but {} does not exist",
                    path.display()
                ),
            ),
            _ => (
                CheckStatus::Fail,
                "WAYLAND_DISPLAY is not set, so every launched application will \
                 exit immediately. Sway exports it to the systemd user session with \
                 `systemctl --user import-environment`, which is exactly what the \
                 sway-config fix adds — apply that check's fix, reload sway, then \
                 restart Suede."
                    .to_string(),
            ),
        };

        self.check(
            ids::WAYLAND_DISPLAY,
            "Applications can reach Wayland",
            status,
            detail,
            Some("troubleshooting/#a-browser-will-not-start"),
        )
    }

    /// Spanning a window across outputs can silently mirror instead.
    ///
    /// When wlroots hands a fullscreen client buffer straight to the display
    /// controller, each output scans that buffer out from its own origin — so
    /// a window that genuinely covers the whole layout shows the *same*
    /// left-hand region on every display. Sway reports everything as correct,
    /// which makes this near-impossible to diagnose from the API alone.
    /// Observed with the Nvidia proprietary driver; the compositor-side
    /// workaround is `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1`.
    fn check_direct_scanout(&self) -> Check {
        let spanning: Vec<String> = self
            .store
            .get()
            .apps
            .into_iter()
            .filter(|app| app.enabled && app.span_outputs)
            .map(|app| app.id)
            .collect();

        let disabled = compositor_env("WLR_SCENE_DISABLE_DIRECT_SCANOUT")
            .is_some_and(|value| value != "0" && !value.is_empty());

        let (status, detail) = match (spanning.is_empty(), disabled) {
            (_, true) => (
                CheckStatus::Pass,
                "direct scanout is disabled, so a spanned window covers every output".to_string(),
            ),
            (true, false) => (
                CheckStatus::Pass,
                "no app spans outputs, so direct scanout is harmless".to_string(),
            ),
            (false, false) => (
                CheckStatus::Warn,
                format!(
                    "{} spans every output, but the compositor was started with direct \
                     scanout enabled. On some drivers (notably Nvidia's) this makes each \
                     display show the same part of the window instead of its own — the \
                     window is the right size, it simply renders wrong. Start sway with \
                     WLR_SCENE_DISABLE_DIRECT_SCANOUT=1.",
                    spanning.join(", ")
                ),
            ),
        };

        let mut check = self.check(
            ids::DIRECT_SCANOUT,
            "Spanning renders across outputs",
            status,
            detail,
            Some("troubleshooting/#a-spanned-window-mirrors-instead-of-spanning"),
        );
        if check.status != CheckStatus::Pass {
            check.fix_available = true;
            check.fix_description = Some(
                "Write a systemd drop-in setting WLR_SCENE_DISABLE_DIRECT_SCANOUT=1 on the                  compositor's unit. You then restart the compositor yourself, since that                  tears down every window."
                    .to_string(),
            );
        }
        check
    }

    /// An appliance whose compositor drives no physical display shows nothing,
    /// however healthy everything else looks.
    ///
    /// wlroots names outputs after its backend: `HEADLESS-n` when synthesising
    /// them, `WL-n` when nested inside another compositor, `X11-n` under X.
    /// Real connectors are `DP-1`, `HDMI-A-1`, `eDP-1` and so on. Getting this
    /// wrong is easy — a compositor that inherits `WAYLAND_DISPLAY` will nest
    /// silently rather than take over the GPU.
    async fn check_real_displays(&self) -> Check {
        let outputs = self.sway.get_outputs().await.unwrap_or_default();
        let synthetic: Vec<&str> = outputs
            .iter()
            .filter(|o| is_synthetic_output(&o.name))
            .map(|o| o.name.as_str())
            .collect();
        let real: Vec<&str> = outputs
            .iter()
            .filter(|o| !is_synthetic_output(&o.name))
            .map(|o| o.name.as_str())
            .collect();

        let (status, detail) = if outputs.is_empty() {
            (
                CheckStatus::Warn,
                "the compositor reports no outputs at all".to_string(),
            )
        } else if real.is_empty() {
            (
                CheckStatus::Warn,
                format!(
                    "every output is synthetic ({}), so nothing reaches a physical \
                     display. The compositor is running headless or nested inside \
                     another one — usually because it inherited WAYLAND_DISPLAY \
                     instead of taking the DRM backend. Start it with \
                     WLR_BACKENDS=drm and an empty WAYLAND_DISPLAY.",
                    synthetic.join(", ")
                ),
            )
        } else if synthetic.is_empty() {
            (
                CheckStatus::Pass,
                format!(
                    "driving {} physical output(s): {}",
                    real.len(),
                    real.join(", ")
                ),
            )
        } else {
            (
                CheckStatus::Pass,
                format!(
                    "driving {}; also present: {}",
                    real.join(", "),
                    synthetic.join(", ")
                ),
            )
        };

        self.check(
            ids::REAL_DISPLAYS,
            "Compositor drives real displays",
            status,
            detail,
            Some("troubleshooting/#a-display-stays-dark"),
        )
    }

    /// Hardware video decode fails *silently*: Chromium asks for VA-API, finds
    /// no driver for the GPU, and quietly decodes on the CPU. Nothing errors,
    /// so the only symptom is a hot CPU and dropped frames on the displays.
    fn check_video_decode(&self) -> Check {
        let vendors = gpu_vendors();
        if vendors.is_empty() {
            return self.check(
                ids::VIDEO_DECODE,
                "Hardware video decode",
                CheckStatus::Warn,
                "could not identify the GPU, so decode support is unknown".to_string(),
                Some("configuration/#environment-and-hardware-acceleration"),
            );
        }

        let mut satisfied = Vec::new();
        let mut missing = Vec::new();
        for vendor in &vendors {
            match vendor.drivers.iter().find(|d| vaapi_driver_present(d)) {
                Some(found) => satisfied.push(format!("{} via {found}", vendor.name)),
                None => missing.push(vendor),
            }
        }

        let (status, detail) = if missing.is_empty() {
            let mut detail = format!("VA-API driver present for {}", satisfied.join(", "));
            // Hard-won: the driver being installed is not the same as the
            // browser using it.
            if vendors.iter().any(|v| v.name == "NVIDIA") {
                detail.push_str(
                    ". Note that Chromium often still decodes in software on NVIDIA even \
                     with nvidia-vaapi-driver installed — confirm with \
                     navigator.mediaCapabilities.decodingInfo(), whose powerEfficient flag \
                     is the honest answer",
                );
                return self.check(
                    ids::VIDEO_DECODE,
                    "Hardware video decode",
                    CheckStatus::Warn,
                    detail,
                    Some("configuration/#environment-and-hardware-acceleration"),
                );
            }
            (CheckStatus::Pass, detail)
        } else {
            let advice: Vec<String> = missing
                .iter()
                .map(|v| format!("{} needs `{}`", v.name, v.package))
                .collect();
            (
                CheckStatus::Warn,
                format!(
                    "no VA-API driver for {}. Video will decode on the CPU, silently: {}",
                    missing
                        .iter()
                        .map(|v| v.name)
                        .collect::<Vec<_>>()
                        .join(", "),
                    advice.join("; ")
                ),
            )
        };

        self.check(
            ids::VIDEO_DECODE,
            "Hardware video decode",
            status,
            detail,
            Some("configuration/#environment-and-hardware-acceleration"),
        )
    }

    /// Sway draws output backgrounds by running `swaybg`. Without it the
    /// `bg` command succeeds and nothing appears, which is the worst
    /// combination: a screen that stays black with no error anywhere.
    async fn check_swaybg(&self) -> Check {
        let desired = self.store.get();
        let wanted = desired
            .outputs
            .iter()
            .filter(|output| {
                output
                    .background
                    .as_ref()
                    .and_then(|reference| reference.resolve(&desired.backgrounds))
                    .is_some()
            })
            .count();
        let installed = crate::supervisor::launcher::resolve_program(&["swaybg".to_string()]);

        let (status, detail) = match (wanted, &installed) {
            (0, Some(path)) => (
                CheckStatus::Pass,
                format!("available at {}", path.display()),
            ),
            (0, None) => (
                CheckStatus::Pass,
                "not installed, but no output asks for a background".to_string(),
            ),
            (n, Some(path)) => (
                CheckStatus::Pass,
                format!("{n} output(s) use a background; swaybg is at {}", path.display()),
            ),
            (n, None) => (
                CheckStatus::Fail,
                format!(
                    "{n} output(s) configure a background but swaybg is not installed, so                      sway will accept the command and draw nothing. Install `swaybg`."
                ),
            ),
        };

        self.check(
            ids::SWAYBG,
            "Backgrounds can be drawn",
            status,
            detail,
            Some("configuration/#backgrounds-and-wallpapers"),
        )
    }

    /// Browsers must be *functional*, not merely present on `PATH`.
    async fn check_browsers(&self) -> Check {
        let mut working = Vec::new();
        let mut broken = Vec::new();

        // The same candidates the launcher presets try, so this check can never
        // disagree with what would actually be launched.
        for family in [
            crate::supervisor::launcher::CHROMIUM_PROGRAMS,
            crate::supervisor::launcher::FIREFOX_PROGRAMS,
        ] {
            let candidates: Vec<String> = family.iter().map(|p| p.to_string()).collect();
            let Some(found) = crate::supervisor::launcher::resolve_program(&candidates) else {
                continue;
            };
            let program = found.display().to_string();
            match run(&program, &["--version"]).await {
                Ok(output) if output.success => {
                    working.push(format!("{program} ({})", first_line(&output.stdout)))
                }
                Ok(output) => broken.push(format!(
                    "{program} exited {}: {}",
                    output.code.unwrap_or(-1),
                    first_line(&output.stderr)
                )),
                Err(error) => broken.push(format!("{program} could not be run: {error}")),
            }
        }

        let (status, detail) = if working.is_empty() && broken.is_empty() {
            (
                CheckStatus::Fail,
                "neither chromium nor firefox is installed".to_string(),
            )
        } else if working.is_empty() {
            (
                CheckStatus::Fail,
                format!("no working browser: {}", broken.join("; ")),
            )
        } else if broken.is_empty() {
            (
                CheckStatus::Pass,
                format!("available: {}", working.join(", ")),
            )
        } else {
            (
                CheckStatus::Warn,
                format!(
                    "available: {}; problems: {}",
                    working.join(", "),
                    broken.join("; ")
                ),
            )
        };

        self.check(
            ids::BROWSERS,
            "Browsers usable",
            status,
            detail,
            Some("getting-started/#browsers"),
        )
    }

    async fn check_pipewire(&self) -> Check {
        // Sink enumeration and per-app routing both go through pipewire-pulse.
        let dump_works = self.audio.is_available() || self.audio.refresh().await.is_ok();
        let pulse_socket = crate::util::runtime_dir()
            .map(|dir| dir.join("pulse/native").exists())
            .unwrap_or(false);

        let (status, detail) = match (dump_works, pulse_socket) {
            (true, true) => (
                CheckStatus::Pass,
                format!(
                    "{} sinks visible; pipewire-pulse is serving",
                    self.audio.sinks().len()
                ),
            ),
            (true, false) => (
                CheckStatus::Warn,
                "pipewire is running but pipewire-pulse is not; audio routing will not work"
                    .to_string(),
            ),
            (false, _) => (
                CheckStatus::Fail,
                "pw-dump is unavailable; audio features are disabled".to_string(),
            ),
        };

        let mut check = self.check(
            ids::PIPEWIRE,
            "PipeWire audio",
            status,
            detail,
            Some("getting-started/#audio"),
        );
        if check.status != CheckStatus::Pass {
            check.fix_available = true;
            check.fix_description = Some(
                "Start the PipeWire user units (pipewire, wireplumber, pipewire-pulse).                  These belong to the session user, so no privileges are needed."
                    .to_string(),
            );
        }
        check
    }

    async fn check_systemd_unit(&self) -> Check {
        let enabled = run("systemctl", &["--user", "is-enabled", UNIT_NAME])
            .await
            .map(|output| first_line(&output.stdout) == "enabled")
            .unwrap_or(false);

        if enabled {
            return self.check(
                ids::SYSTEMD_UNIT,
                "Service starts with the session",
                CheckStatus::Pass,
                format!("{UNIT_NAME} is enabled"),
                None,
            );
        }

        let mut check = self.check(
            ids::SYSTEMD_UNIT,
            "Service starts with the session",
            CheckStatus::Fail,
            format!("{UNIT_NAME} is not enabled, so Suede will not start after a reboot"),
            Some("getting-started/#service"),
        );
        check.fix_available = true;
        check.fix_description = Some(format!(
            "Write ~/.config/systemd/user/{UNIT_NAME} if absent, then enable it \
             against sway-session.target"
        ));
        check
    }

    fn check_sway_config(&self) -> Check {
        let path = self.bootstrap.sway_config_path.clone();
        let text = std::fs::read_to_string(&path).unwrap_or_default();

        if config_block::has_block(&text) {
            return self.check(
                ids::SWAY_CONFIG,
                "Sway configuration prepared",
                CheckStatus::Pass,
                format!("managed block present in {}", path.display()),
                None,
            );
        }

        let mut check = self.check(
            ids::SWAY_CONFIG,
            "Sway configuration prepared",
            CheckStatus::Warn,
            format!(
                "{} has no Suede block, so the session environment may not reach systemd",
                path.display()
            ),
            Some("getting-started/#sway-configuration"),
        );
        check.fix_available = true;
        check.fix_description = Some(format!(
            "Append a marker-delimited block to {}. Content outside the markers is untouched.",
            path.display()
        ));
        check
    }

    fn check_state_dir(&self) -> Check {
        let dir = &self.bootstrap.state_dir;
        let writable = std::fs::create_dir_all(dir)
            .and_then(|_| {
                let probe = dir.join(".write-probe");
                std::fs::write(&probe, b"")?;
                std::fs::remove_file(&probe)
            })
            .is_ok();

        let (status, detail) = if writable {
            (CheckStatus::Pass, format!("{} is writable", dir.display()))
        } else {
            (
                CheckStatus::Fail,
                format!(
                    "{} is not writable; configuration cannot be saved",
                    dir.display()
                ),
            )
        };
        self.check(
            ids::STATE_DIR,
            "State directory writable",
            status,
            detail,
            None,
        )
    }

    /// Whether anything outside this machine can actually reach the API.
    ///
    /// Added because a firewall silently dropping the API port is
    /// indistinguishable, from the outside, from an appliance that is dead:
    /// the connection times out rather than being refused, and every other
    /// check passes because they all run from inside.
    async fn check_api_reachability(&self) -> Check {
        let bind = self.bootstrap.bind;
        let mut firewall = HostFirewall::None;
        if !bind.ip().is_loopback() {
            for candidate in [
                HostFirewall::Ufw,
                HostFirewall::Firewalld,
                HostFirewall::Nftables,
            ] {
                let Some(unit) = candidate.unit() else {
                    continue;
                };
                if let Ok(output) = run("systemctl", &["is-active", unit]).await {
                    if first_line(&output.stdout) == "active" {
                        firewall = candidate;
                        break;
                    }
                }
            }
        }

        let (status, detail) = assess_reachability(
            bind,
            firewall,
            self.bootstrap.auth_enabled(),
            self.last_remote_client(),
        );
        self.check(
            ids::API_REACHABILITY,
            "API reachable from the network",
            status,
            detail,
            Some("getting-started/#network-access"),
        )
    }

    fn check(
        &self,
        id: &str,
        title: &str,
        status: CheckStatus,
        detail: String,
        docs: Option<&str>,
    ) -> Check {
        Check {
            id: id.to_string(),
            title: title.to_string(),
            status,
            detail,
            docs_url: docs.map(|path| self.bootstrap.docs_url(path)),
            fix_available: false,
            fix_description: None,
        }
    }

    // --- remediations -----------------------------------------------------

    /// Run the remediation for `id`, returning what was done.
    pub async fn fix(&self, id: &str) -> ApiResult<String> {
        let outcome = match id {
            ids::SYSTEMD_UNIT => self.fix_systemd_unit().await?,
            ids::SWAY_CONFIG => self.fix_sway_config()?,
            ids::PIPEWIRE => self.fix_pipewire().await?,
            ids::DIRECT_SCANOUT => self.fix_direct_scanout().await?,
            other => {
                return Err(ApiError::NotFound(format!(
                    "no automated fix is available for check {other:?}"
                )))
            }
        };
        self.run_all().await;
        Ok(outcome)
    }

    async fn fix_systemd_unit(&self) -> ApiResult<String> {
        let mut steps = Vec::new();

        let packaged = std::path::Path::new("/usr/lib/systemd/user").join(UNIT_NAME);
        let user_unit = self.bootstrap.systemd_user_dir.join(UNIT_NAME);

        if !packaged.exists() && !user_unit.exists() {
            let executable = std::env::current_exe()
                .map_err(|error| ApiError::Internal(format!("cannot locate suede: {error}")))?;
            let unit = unit_file(&executable.display().to_string());
            if let Some(parent) = user_unit.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ApiError::Internal(format!("cannot create {}: {error}", parent.display()))
                })?;
            }
            std::fs::write(&user_unit, unit).map_err(|error| {
                ApiError::Internal(format!("cannot write {}: {error}", user_unit.display()))
            })?;
            steps.push(format!("wrote {}", user_unit.display()));
        }

        run("systemctl", &["--user", "daemon-reload"])
            .await
            .map_err(|error| {
                ApiError::Internal(format!("systemctl daemon-reload failed: {error}"))
            })?;
        steps.push("reloaded the systemd user daemon".to_string());

        let output = run("systemctl", &["--user", "enable", UNIT_NAME])
            .await
            .map_err(|error| ApiError::Internal(format!("systemctl enable failed: {error}")))?;
        if !output.success {
            return Err(ApiError::Internal(format!(
                "systemctl enable {UNIT_NAME} failed: {}",
                first_line(&output.stderr)
            )));
        }
        steps.push(format!("enabled {UNIT_NAME}"));

        Ok(steps.join("; "))
    }

    /// PipeWire is a user service, so starting it needs no privileges at all —
    /// it simply has to be asked.
    async fn fix_pipewire(&self) -> ApiResult<String> {
        let mut started = Vec::new();
        for unit in [
            "pipewire.socket",
            "pipewire-pulse.socket",
            "pipewire.service",
            "wireplumber.service",
            "pipewire-pulse.service",
        ] {
            if let Ok(output) = run("systemctl", &["--user", "start", unit]).await {
                if output.success {
                    started.push(unit);
                }
            }
        }
        if started.is_empty() {
            return Err(ApiError::Internal(
                "could not start any PipeWire user unit; is PipeWire installed?".into(),
            ));
        }
        // Give the graph a moment, then re-read it.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = self.audio.refresh().await;
        Ok(format!("started {}", started.join(", ")))
    }

    /// Set `WLR_SCENE_DISABLE_DIRECT_SCANOUT` on the compositor's own unit.
    ///
    /// The variable has to be in the *compositor's* environment, not Suede's,
    /// so the only thing Suede can do without privileges is write a systemd
    /// drop-in and ask for a restart.
    async fn fix_direct_scanout(&self) -> ApiResult<String> {
        let Some(unit) = compositor_unit() else {
            return Err(ApiError::Validation(
                "the compositor is not running as a systemd user unit, so Suede cannot \
                 set its environment. Add WLR_SCENE_DISABLE_DIRECT_SCANOUT=1 wherever \
                 sway is started (provision.sh does this for a standard install)."
                    .into(),
            ));
        };

        let dir = self.bootstrap.systemd_user_dir.join(format!("{unit}.d"));
        std::fs::create_dir_all(&dir).map_err(|error| {
            ApiError::Internal(format!("cannot create {}: {error}", dir.display()))
        })?;

        let path = dir.join("10-suede-scanout.conf");
        std::fs::write(
            &path,
            "# Written by Suede.\n\
             # Without this, a window spanning several outputs is handed straight to\n\
             # each display controller, so every screen shows the same part of it.\n\
             [Service]\n\
             Environment=WLR_SCENE_DISABLE_DIRECT_SCANOUT=1\n",
        )
        .map_err(|error| ApiError::Internal(format!("cannot write {}: {error}", path.display())))?;

        run("systemctl", &["--user", "daemon-reload"])
            .await
            .map_err(|error| ApiError::Internal(format!("daemon-reload failed: {error}")))?;

        Ok(format!(
            "wrote {} for {unit}. Restart the compositor to apply it \
             (`systemctl --user restart {unit}`) — Suede will not do that itself, \
             because it would tear down every window on every display.",
            path.display()
        ))
    }

    fn fix_sway_config(&self) -> ApiResult<String> {
        let path = self.bootstrap.sway_config_path.clone();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ApiError::Internal(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();

        // Keep a copy: this file belongs to the user, not to Suede.
        if !existing.is_empty() {
            let backup = path.with_extension("suede-backup");
            let _ = std::fs::write(&backup, &existing);
        }

        let updated = config_block::upsert_block(&existing, config_block::SWAY_BLOCK_BODY);
        std::fs::write(&path, updated).map_err(|error| {
            ApiError::Internal(format!("cannot write {}: {error}", path.display()))
        })?;

        Ok(format!(
            "updated the Suede block in {}; reload sway to apply",
            path.display()
        ))
    }
}

fn unit_file(executable: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Suede display appliance daemon\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={executable} run\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=sway-session.target\n"
    )
}

struct CommandOutput {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run a program with a timeout, so a hung tool cannot stall the checks.
async fn run(program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
    let future = tokio::process::Command::new(program).args(args).output();

    let output = tokio::time::timeout(COMMAND_TIMEOUT, future)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"))??;

    Ok(CommandOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Outputs invented by the compositor rather than driven from a connector.
pub fn is_synthetic_output(name: &str) -> bool {
    name.starts_with("HEADLESS-") || name.starts_with("WL-") || name.starts_with("X11-")
}

/// A GPU vendor, and what it needs for hardware video decode.
pub struct GpuVendor {
    pub name: &'static str,
    /// VA-API driver filenames that would satisfy it, most preferred first.
    pub drivers: &'static [&'static str],
    /// The package an operator would install.
    pub package: &'static str,
}

/// Which GPU vendors are present, from the PCI IDs of the DRM devices.
///
/// A production appliance may well have Intel integrated graphics where the
/// demo machine has a discrete card, and the right driver differs.
pub fn gpu_vendors() -> Vec<GpuVendor> {
    let mut found: Vec<GpuVendor> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Cards only: `card0`, not connectors like `card0-DP-1`.
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let Ok(id) = std::fs::read_to_string(entry.path().join("device/vendor")) else {
            continue;
        };
        let vendor = match id.trim() {
            "0x8086" => GpuVendor {
                name: "Intel",
                drivers: &["iHD_drv_video.so", "i965_drv_video.so"],
                package: "intel-media-va-driver-non-free",
            },
            "0x10de" => GpuVendor {
                name: "NVIDIA",
                drivers: &["nvidia_drv_video.so"],
                package: "nvidia-vaapi-driver",
            },
            "0x1002" | "0x1022" => GpuVendor {
                name: "AMD",
                drivers: &["radeonsi_drv_video.so", "r600_drv_video.so"],
                package: "mesa-va-drivers",
            },
            _ => continue,
        };
        if !found.iter().any(|v| v.name == vendor.name) {
            found.push(vendor);
        }
    }
    found
}

/// Whether a VA-API driver library is installed.
///
/// Detected by capability rather than package name: distributions rename these
/// packages (Ubuntu 26.04 ships `libva2t64`, not `libva2`), so asking dpkg
/// gives the wrong answer.
pub fn vaapi_driver_present(driver: &str) -> bool {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(custom) = std::env::var("LIBVA_DRIVERS_PATH") {
        roots.extend(custom.split(':').map(std::path::PathBuf::from));
    }
    for root in [
        "/usr/lib/x86_64-linux-gnu/dri",
        "/usr/lib/aarch64-linux-gnu/dri",
        "/usr/lib/dri",
        "/usr/lib64/dri",
    ] {
        roots.push(std::path::PathBuf::from(root));
    }
    roots.iter().any(|root| root.join(driver).exists())
}

/// The systemd user unit the compositor runs under, if any.
fn compositor_unit() -> Option<String> {
    let socket = crate::sway::discover_socket()?;
    let name = socket.file_name()?.to_str()?;
    let pid: u32 = name.split('.').nth_back(1)?.parse().ok()?;
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup
        .lines()
        .find_map(|line| line.rsplit('/').next())
        .filter(|unit| unit.ends_with(".service"))
        .map(str::to_string)
}

/// Read a variable from the compositor's own environment.
///
/// Sway's IPC socket is named `sway-ipc.<uid>.<pid>.sock`, which is how the
/// running compositor can be identified without any extra plumbing.
fn compositor_env(key: &str) -> Option<String> {
    let socket = crate::sway::discover_socket()?;
    let name = socket.file_name()?.to_str()?;
    let pid: u32 = name.split('.').nth_back(1)?.parse().ok()?;
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    String::from_utf8_lossy(&environ)
        .split('\0')
        .find_map(|entry| entry.strip_prefix(&format!("{key}="))?.to_string().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::MockAudio;
    use crate::sway::mock::MockSway;

    fn runner(state_dir: std::path::PathBuf) -> CheckRunner {
        let store = Arc::new(crate::state::StateStore::ephemeral(state_dir.clone()));
        // Redirect everything a fix might write, so no test can touch a real
        // home directory or a real systemd unit.
        let bootstrap = Arc::new(BootstrapConfig {
            sway_config_path: state_dir.join("sway/config"),
            systemd_user_dir: state_dir.join("systemd/user"),
            state_dir,
            ..BootstrapConfig::default()
        });
        CheckRunner::new(
            bootstrap,
            Arc::new(MockSway::with_fixtures()),
            Arc::new(MockAudio::with_sinks()),
            store,
            EventHub::new(),
        )
    }

    #[tokio::test]
    async fn every_check_reports_something() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        assert_eq!(checks.len(), 13);
        for id in [
            ids::SWAY_SOCKET,
            ids::SWAY_VERSION,
            ids::BROWSERS,
            ids::PIPEWIRE,
            ids::SYSTEMD_UNIT,
            ids::SWAY_CONFIG,
            ids::STATE_DIR,
            ids::DIRECT_SCANOUT,
            ids::REAL_DISPLAYS,
            ids::VIDEO_DECODE,
            ids::SWAYBG,
            ids::API_REACHABILITY,
        ] {
            assert!(checks.iter().any(|check| check.id == id), "missing {id}");
        }
    }

    #[tokio::test]
    async fn writable_state_directory_passes() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        let check = checks.iter().find(|c| c.id == ids::STATE_DIR).unwrap();
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn version_check_passes_on_a_recent_sway() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        let check = checks.iter().find(|c| c.id == ids::SWAY_VERSION).unwrap();
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn version_check_warns_on_an_older_sway() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::empty());
        sway.set_version(crate::sway::SwayVersion {
            major: 1,
            minor: 8,
            patch: 0,
            human_readable: None,
        });
        let runner = CheckRunner::new(
            bootstrap,
            sway,
            Arc::new(MockAudio::default()),
            Arc::new(crate::state::StateStore::ephemeral(
                dir.path().to_path_buf(),
            )),
            EventHub::new(),
        );
        let checks = runner.run_all().await;
        let check = checks.iter().find(|c| c.id == ids::SWAY_VERSION).unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("1.10"));
    }

    #[tokio::test]
    async fn failing_checks_carry_documentation_links() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        // Browsers are absent in CI, so this check fails and must be actionable.
        let browsers = checks.iter().find(|c| c.id == ids::BROWSERS).unwrap();
        if browsers.status != CheckStatus::Pass {
            assert!(browsers.docs_url.is_some());
        }
    }

    #[tokio::test]
    async fn fixes_are_offered_only_where_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        for check in &checks {
            if check.fix_available {
                assert!(
                    check.fix_description.is_some(),
                    "{} offers a fix without describing it",
                    check.id
                );
            }
        }
        // Package installation needs root and is never offered.
        let browsers = checks.iter().find(|c| c.id == ids::BROWSERS).unwrap();
        assert!(!browsers.fix_available);
    }

    #[tokio::test]
    async fn unknown_fix_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let error = runner(dir.path().to_path_buf())
            .fix("no-such-check")
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::NotFound(_)));
    }

    #[tokio::test]
    async fn checks_publish_only_when_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let mut receiver = runner.events.subscribe();

        runner.run_all().await;
        assert!(receiver.try_recv().is_ok(), "first run should publish");

        runner.run_all().await;
        assert!(
            receiver.try_recv().is_err(),
            "an unchanged second run should stay quiet"
        );
    }

    #[test]
    fn synthetic_outputs_are_recognised() {
        // These are the names wlroots gives when it is not driving a connector.
        for name in ["HEADLESS-1", "WL-2", "X11-1"] {
            assert!(is_synthetic_output(name), "{name} should be synthetic");
        }
        for name in ["DP-1", "HDMI-A-2", "eDP-1", "DVI-D-1"] {
            assert!(!is_synthetic_output(name), "{name} is a real connector");
        }
    }

    #[tokio::test]
    async fn a_headless_compositor_is_flagged() {
        // The fixtures are HDMI connectors, so this passes; swap in a headless
        // set and the check must warn that nothing reaches a display.
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::empty());
        sway.set_outputs(vec![crate::model::Output {
            name: "HEADLESS-1".into(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: Default::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }]);
        let runner = CheckRunner::new(
            bootstrap,
            sway,
            Arc::new(MockAudio::default()),
            Arc::new(crate::state::StateStore::ephemeral(
                dir.path().to_path_buf(),
            )),
            EventHub::new(),
        );
        let checks = runner.run_all().await;
        let check = checks
            .iter()
            .find(|c| c.id == ids::REAL_DISPLAYS)
            .expect("real-displays check");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("synthetic"));
    }

    #[tokio::test]
    async fn real_connectors_pass() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        let check = checks.iter().find(|c| c.id == ids::REAL_DISPLAYS).unwrap();
        // The fixtures are HDMI-A-*, which are genuine connectors.
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn video_decode_is_reported_for_this_machine() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        let check = checks.iter().find(|c| c.id == ids::VIDEO_DECODE).unwrap();
        // Whatever the host, the check must say something actionable.
        assert!(!check.detail.is_empty());
        assert!(check.docs_url.is_some());
    }

    #[test]
    fn every_gpu_vendor_names_a_package_to_install() {
        // The production appliance may have Intel where the demo box has NVIDIA.
        for vendor in gpu_vendors() {
            assert!(!vendor.package.is_empty(), "{} has no package", vendor.name);
            assert!(!vendor.drivers.is_empty(), "{} has no driver", vendor.name);
        }
    }

    #[tokio::test]
    async fn fixable_checks_all_have_a_handler() {
        // A check advertising a fix that the endpoint rejects would be worse
        // than offering none at all. Asserted against the dispatch table rather
        // than by invoking the fixes, which write real files and touch systemd.
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        for check in runner.run_all().await.iter().filter(|c| c.fix_available) {
            assert!(
                FIXABLE.contains(&check.id.as_str()),
                "{} advertises a fix that fix() does not handle",
                check.id
            );
        }
    }

    #[tokio::test]
    async fn the_sway_config_fix_is_idempotent_and_preserves_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let config = dir.path().join("sway/config");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "set $mod Mod4\nbindsym $mod+Return exec foot\n").unwrap();

        runner.fix(ids::SWAY_CONFIG).await.unwrap();
        let once = std::fs::read_to_string(&config).unwrap();
        assert!(once.contains("set $mod Mod4"), "user content must survive");
        assert!(once.contains("BEGIN SUEDE_CONFIG"));

        runner.fix(ids::SWAY_CONFIG).await.unwrap();
        let twice = std::fs::read_to_string(&config).unwrap();
        assert_eq!(once, twice, "applying the fix twice must change nothing");
        assert_eq!(twice.matches("BEGIN SUEDE_CONFIG").count(), 1);
    }

    #[tokio::test]
    async fn the_sway_config_fix_backs_up_what_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let config = dir.path().join("sway/config");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "precious\n").unwrap();

        runner.fix(ids::SWAY_CONFIG).await.unwrap();
        let backup = config.with_extension("suede-backup");
        assert!(backup.exists(), "the user's file must be kept");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "precious\n");
    }

    #[tokio::test]
    async fn the_scanout_fix_reports_when_it_cannot_help() {
        // With no compositor running as a unit, the fix must explain what to do
        // rather than silently writing a drop-in nothing will read.
        let dir = tempfile::tempdir().unwrap();
        let result = runner(dir.path().to_path_buf())
            .fix(ids::DIRECT_SCANOUT)
            .await;
        if let Err(error) = result {
            assert!(matches!(error, ApiError::Validation(_)));
            assert!(error
                .to_string()
                .contains("WLR_SCENE_DISABLE_DIRECT_SCANOUT"));
        }
    }

    #[test]
    fn every_fixable_id_is_dispatched() {
        // Guards the other direction: an id in FIXABLE that fix() forgot.
        for id in FIXABLE {
            assert!(
                [
                    ids::SYSTEMD_UNIT,
                    ids::SWAY_CONFIG,
                    ids::PIPEWIRE,
                    ids::DIRECT_SCANOUT
                ]
                .contains(id),
                "{id} is advertised as fixable but not dispatched"
            );
        }
    }

    #[test]
    fn generated_unit_starts_with_the_session() {
        let unit = unit_file("/usr/bin/suede");
        assert!(unit.contains("ExecStart=/usr/bin/suede run"));
        assert!(unit.contains("WantedBy=sway-session.target"));
    }

    // --- API reachability -------------------------------------------------

    fn addr(text: &str) -> std::net::SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_loopback_bind_explains_how_to_reach_it_anyway() {
        let (status, detail) =
            assess_reachability(addr("127.0.0.1:9088"), HostFirewall::None, false, None);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("only on this machine"));
        // The tunnel command is the answer to "why can I not open the page",
        // so it belongs in the detail rather than only in the documentation.
        assert!(detail.contains("ssh -L 9088:127.0.0.1:9088"));
    }

    #[test]
    fn an_exposed_bind_with_no_firewall_passes() {
        let (status, detail) =
            assess_reachability(addr("0.0.0.0:9088"), HostFirewall::None, true, None);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("no host firewall"));
        assert!(detail.contains("bearer token"));
    }

    #[test]
    fn an_exposed_bind_behind_a_firewall_warns_with_the_command_to_open_it() {
        // The case that cost an afternoon: bound to the world, dropped by ufw,
        // every other check green because they all run from inside.
        let (status, detail) =
            assess_reachability(addr("0.0.0.0:7075"), HostFirewall::Ufw, false, None);
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("ufw is running"));
        assert!(detail.contains("sudo ufw allow 7075/tcp"));
        assert!(
            detail.contains("looks exactly like a daemon that is not running"),
            "the symptom matters more than the cause: {detail}"
        );
    }

    #[test]
    fn firewalld_gets_its_own_command() {
        let (_, detail) =
            assess_reachability(addr("0.0.0.0:9000"), HostFirewall::Firewalld, false, None);
        assert!(detail.contains("firewall-cmd --permanent --add-port=9000/tcp"));
    }

    #[test]
    fn nftables_is_sent_to_the_documentation_rather_than_given_a_wrong_command() {
        // The rule depends on the table and chain names in use, so any
        // one-liner Suede printed would be a guess.
        assert_eq!(HostFirewall::Nftables.allow_command(9088), None);
        let (status, detail) =
            assess_reachability(addr("0.0.0.0:9088"), HostFirewall::Nftables, false, None);
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("Open it in the ruleset"));
    }

    #[test]
    fn an_unauthenticated_exposed_bind_says_so() {
        let (_, detail) =
            assess_reachability(addr("0.0.0.0:9088"), HostFirewall::None, false, None);
        assert!(detail.contains("full control"));
    }

    #[test]
    fn a_connection_from_off_box_clears_the_firewall_warning() {
        // Direct evidence beats inference: the rules cannot be read, but a
        // request that crossed the network settles the question. Without this
        // the warning could never be cleared, and an alert that never clears
        // is one the operator learns to scroll past.
        let peer = Some("10.0.0.5".parse().unwrap());
        let (status, detail) =
            assess_reachability(addr("0.0.0.0:9088"), HostFirewall::Ufw, false, peer);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("confirmed reachable"));
        assert!(detail.contains("10.0.0.5"));
    }

    #[test]
    fn the_warning_says_it_will_clear_itself() {
        let (_, detail) = assess_reachability(addr("0.0.0.0:9088"), HostFirewall::Ufw, false, None);
        assert!(detail.contains("clears itself"));
    }

    #[test]
    fn loopback_clients_are_not_evidence_of_anything() {
        let runner = runner(tempfile::tempdir().unwrap().path().to_path_buf());
        for local in ["127.0.0.1", "::1"] {
            runner.note_client(local.parse().unwrap());
        }
        assert_eq!(
            runner.last_remote_client(),
            None,
            "the daemon's own probes and page heartbeats must not count"
        );

        runner.note_client("192.168.1.20".parse().unwrap());
        assert_eq!(
            runner.last_remote_client(),
            Some("192.168.1.20".parse().unwrap())
        );
    }

    #[test]
    fn a_loopback_bind_never_reports_a_firewall_problem() {
        // Nothing can be filtering loopback traffic in a way that matters, so
        // a warning here would be noise the operator learns to ignore.
        for firewall in [
            HostFirewall::Ufw,
            HostFirewall::Firewalld,
            HostFirewall::Nftables,
        ] {
            let (status, _) = assess_reachability(addr("127.0.0.1:9088"), firewall, false, None);
            assert_eq!(status, CheckStatus::Pass, "{firewall:?} should not warn");
        }
    }
}
