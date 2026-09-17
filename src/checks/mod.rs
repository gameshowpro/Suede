//! Environment health checks and their remediations.
//!
//! Suede verifies the environment it depends on rather than silently mutating
//! it. Failures carry a documentation link; only fixes within the session
//! user's power are offered, and none ever run implicitly.

pub mod config_block;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use crate::audio::AudioMonitor;
use crate::config::BootstrapConfig;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventHub, ServerEvent};
use crate::model::{
    format_refresh, Check, CheckStatus, Output, OutputConfig, OutputTiming, PackageVersion,
    ProjectionStats,
};
use crate::reconciler::plan::{plan_outputs, Capabilities};
use crate::reconciler::ReconcileTrigger;
use crate::snapshot::Snapshot;
use crate::sway::{SwayClient, SwayResult};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Where ALSA exposes its device nodes.
const SOUND_DEVICES: &str = "/dev/snd";

/// What the sound devices themselves say, when PipeWire offers no output.
///
/// "No sinks" has three quite different causes and one useless summary, so
/// the check asks the kernel directly rather than guessing. Opening a
/// control node read-only is what any mixer does; it takes nothing
/// exclusively and disturbs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoundDevices {
    /// No control nodes at all: no sound hardware, or no driver bound.
    Absent,
    /// Nodes exist, but this user may not open them.
    Denied,
    /// Nodes exist and open, so silence has some other cause.
    Available,
}

fn sound_devices() -> SoundDevices {
    sound_devices_in(SOUND_DEVICES)
}

fn sound_devices_in(root: &str) -> SoundDevices {
    let Ok(entries) = std::fs::read_dir(root) else {
        return SoundDevices::Absent;
    };
    let controls: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("control"))
        })
        .collect();
    if controls.is_empty() {
        SoundDevices::Absent
    } else if controls
        .iter()
        .any(|path| std::fs::File::open(path).is_ok())
    {
        SoundDevices::Available
    } else {
        SoundDevices::Denied
    }
}
const UNIT_NAME: &str = "suede.service";

/// Checks that `fix()` can remediate. Kept beside the dispatch so the two
/// cannot drift apart.
pub const FIXABLE: &[&str] = &[
    ids::SYSTEMD_UNIT,
    ids::SWAY_CONFIG,
    ids::PIPEWIRE,
    ids::DIRECT_SCANOUT,
    ids::OUTPUT_PHASE,
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
    pub const DECODE_MEASURED: &str = "decode-measured";
    pub const CAPTURE_DEVICES: &str = "capture-devices";
    pub const REFRESH_RATES: &str = "refresh-rates";
    pub const OUTPUT_PHASE: &str = "output-phase";

    /// Every check `run_all` runs, in the order it runs them — kept here so
    /// the count carried by `every_check_reports_something`,
    /// `runs_health_checks` and `scripts/smoke-test.sh` has exactly one
    /// place that can drift from reality (the Rust two check against this
    /// list directly; the shell script still carries its own number).
    pub const ALL: &[&str] = &[
        SWAY_SOCKET,
        SWAY_VERSION,
        WAYLAND_DISPLAY,
        DIRECT_SCANOUT,
        REAL_DISPLAYS,
        REFRESH_RATES,
        OUTPUT_PHASE,
        VIDEO_DECODE,
        SWAYBG,
        BROWSERS,
        PIPEWIRE,
        SYSTEMD_UNIT,
        SWAY_CONFIG,
        STATE_DIR,
        API_REACHABILITY,
        CAPTURE_DEVICES,
        DECODE_MEASURED,
    ];
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
    /// Observed state, read by the `output-phase` check (the slicer's own
    /// `phaseMs` measurement) and by its fix (which outputs are active).
    snapshot: Arc<Snapshot>,
    /// Lets the `output-phase` fix ask for a reconciliation pass once it has
    /// re-enabled the outputs it disabled, so anything downstream of them
    /// (window placement, the applied-settings cache) catches up.
    trigger: ReconcileTrigger,
    results: RwLock<Vec<Check>>,
    /// Most recent client that was not on this machine. See [`note_client`].
    ///
    /// [`note_client`]: CheckRunner::note_client
    last_remote_client: RwLock<Option<std::net::IpAddr>>,
    /// Versions from browser probes that succeeded, keyed by path and mtime.
    ///
    /// The probe races whatever else the machine is doing, and a `--version`
    /// that took over five seconds under load says nothing about the binary.
    /// Without this memory a busy appliance flapped the browsers check
    /// between pass and fail, republishing to every client each time - and
    /// the same race failed CI whenever the runner was slow at the wrong
    /// moment. The mtime keys the answer to the exact file that gave it, so
    /// an upgraded browser is probed afresh rather than credited with its
    /// predecessor's health.
    probed_versions: RwLock<HashMap<PathBuf, (SystemTime, String)>>,
    /// The last browser capability measurement, judged by `decode-measured`.
    capabilities: Arc<crate::capabilities::CapabilityStore>,
}

impl CheckRunner {
    // Eight collaborators, all distinct types (`Arc<dyn Trait>`, `Arc<Store>`,
    // `EventHub`, two `Arc<...>` stores, `Arc<Snapshot>`, `ReconcileTrigger`):
    // a transposition would still compile, but wrongly, the same risk
    // `ReconcilerDeps` exists to close off. Left positional rather than
    // following that pattern here because every caller already spells out
    // each argument on its own line; revisit if a ninth collaborator turns up.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bootstrap: Arc<BootstrapConfig>,
        sway: Arc<dyn SwayClient>,
        audio: Arc<dyn AudioMonitor>,
        store: Arc<crate::state::StateStore>,
        events: EventHub,
        capabilities: Arc<crate::capabilities::CapabilityStore>,
        snapshot: Arc<Snapshot>,
        trigger: ReconcileTrigger,
    ) -> Self {
        Self {
            bootstrap,
            sway,
            audio,
            store,
            events,
            snapshot,
            trigger,
            results: RwLock::new(Vec::new()),
            last_remote_client: RwLock::new(None),
            probed_versions: RwLock::new(HashMap::new()),
            capabilities,
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
            self.check_refresh_rates().await,
            self.check_output_phase().await,
            self.check_video_decode(),
            self.check_swaybg().await,
            self.check_browsers().await,
            self.check_pipewire().await,
            self.check_systemd_unit().await,
            self.check_sway_config(),
            self.check_state_dir(),
            self.check_api_reachability().await,
            self.check_capture_devices(),
            self.check_decode_measured(),
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

    /// Whether the compositor's direct scanout setting matches the display
    /// path this appliance runs — and the two cases are opposites.
    ///
    /// Tiling (`allow_overlaps = false`): sway hands one spanning window to
    /// every output. When wlroots can pass that fullscreen client buffer
    /// straight to the display controller, each output scans it out from its
    /// *own* origin, so a window covering the whole layout shows the same
    /// left-hand region everywhere. Sway reports it all as correct, which
    /// makes it near-impossible to diagnose from the API alone. Observed with
    /// the Nvidia proprietary driver; the workaround is
    /// `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1`.
    ///
    /// Slicing (`allow_overlaps = true`, `direct_scanout = true`, the
    /// default): no client ever spans the physical outputs. The app renders
    /// into the headless canvas and each display is handed one private,
    /// output-sized slicer buffer — the textbook scanout case, and immune to
    /// that mirroring bug. The variable then costs a full-screen compositor
    /// pass per output per frame and buys nothing.
    ///
    /// Slicing with `direct_scanout = false`: the same layout, deliberately
    /// composited instead of flipped, so the two can be measured against each
    /// other. The expectation flips back to the variable being set, and this
    /// check is what says whether the running compositor actually agrees —
    /// otherwise the A/B compares a machine with itself.
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

        let expectation = ScanoutExpectation::of(&self.bootstrap);
        let (status, detail) = judge_direct_scanout(expectation, &spanning, disabled);

        let mut check = self.check(
            ids::DIRECT_SCANOUT,
            expectation.title(),
            status,
            detail,
            Some(if expectation.allow_overlaps {
                "configuration/#direct-scanout"
            } else {
                "troubleshooting/#a-spanned-window-mirrors-instead-of-spanning"
            }),
        );
        if check.status != CheckStatus::Pass {
            check.fix_available = true;
            check.fix_description = Some(if expectation.scanout_expected() {
                "Remove the systemd drop-in that sets \
                 WLR_SCENE_DISABLE_DIRECT_SCANOUT on the compositor's unit. You then \
                 restart the compositor yourself, since that tears down every window."
                    .to_string()
            } else {
                "Write a systemd drop-in setting WLR_SCENE_DISABLE_DIRECT_SCANOUT=1 \
                 on the compositor's unit. You then restart the compositor \
                 yourself, since that tears down every window."
                    .to_string()
            });
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

    /// Outputs mode-set at different rates drift a frame apart over time — see
    /// [`refresh_rate_verdict`] for the arithmetic and the two projectors this
    /// was written for (60.000 Hz and 59.939 Hz, both advertising both).
    async fn check_refresh_rates(&self) -> Check {
        let outputs = self.sway.get_outputs().await.unwrap_or_default();
        let configured = self.store.effective().outputs;
        let (status, detail) = refresh_rate_verdict(&outputs, &configured);
        self.check(
            ids::REFRESH_RATES,
            "Displays share a refresh rate",
            status,
            detail,
            Some("configuration/#refresh-rates"),
        )
    }

    /// Whether the active displays' vblank rasters share a phase, from the
    /// slicer's own `wp_presentation` measurement (`phaseMs` on each output
    /// of `GET /projection/stats`).
    ///
    /// Measured on a four-projector NVIDIA rig (RTX A1000, sway 1.10.1,
    /// 2026-09-15): outputs enabled one `output … enable` at a time left
    /// the first head 7.1 ms out of phase with the other three, which
    /// locked to each other — about 145 straddled frames (shown on
    /// different refreshes) per 330 captured. The same four, disabled then
    /// re-enabled together in one sway command, landed within 0.03 ms of
    /// each other — about 20 straddles per 330 at the same rate. The rig's
    /// original, unmanaged state showed the same signature: 2.2 ms off.
    /// See [`CheckRunner::fix_output_phase`] for the remedy this offers.
    async fn check_output_phase(&self) -> Check {
        let stats = self.snapshot.projection_stats();
        let running = self.snapshot.slicer_running();
        let (status, detail) = output_phase_verdict(running, stats.as_ref());
        let mut check = self.check(
            ids::OUTPUT_PHASE,
            "Displays are in phase",
            status,
            detail,
            Some("how-it-works/#keeping-the-displays-in-step"),
        );
        if check.status == CheckStatus::Warn {
            check.fix_available = true;
            check.fix_description = Some(
                "Disable every active display and re-enable them together. \
                 The wall goes dark for about three seconds."
                    .to_string(),
            );
        }
        check
    }

    /// Hardware video decode fails *silently*: Chromium asks for VA-API, finds
    /// no driver for the GPU, and quietly decodes on the CPU. Nothing errors,
    /// so the only symptom is a hot CPU and dropped frames on the displays.
    fn check_video_decode(&self) -> Check {
        let vendors = gpu_vendors();
        if vendors.is_empty() {
            // Not every GPU is a PCI device. A Raspberry Pi's VideoCore is a
            // platform device with no vendor ID, and VA-API does not exist on
            // it at all — its decoders are V4L2 devices, which Raspberry Pi
            // OS's Chromium build drives directly. The VA-API probe's "unknown
            // GPU" warning would be one no operator could ever clear.
            if let Some((status, detail)) = videocore_decode() {
                return self.check(
                    ids::VIDEO_DECODE,
                    "Hardware video decode",
                    status,
                    detail,
                    Some("configuration/#environment-and-hardware-acceleration"),
                );
            }
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
            // browser using it — an NVIDIA machine measured software-only
            // through a present, working driver. So when a measurement
            // exists, let it settle the question instead of hedging forever.
            if vendors.iter().any(|v| v.name == "NVIDIA") {
                let (status, verdict) = match self.measured_hardware() {
                    Some(families) if !families.is_empty() => (
                        CheckStatus::Pass,
                        format!(
                            ", and the browser measurably uses it: hardware \
                             decode for {}",
                            families.join(", ")
                        ),
                    ),
                    Some(_) => (
                        CheckStatus::Warn,
                        ", but the measurement shows the browser is NOT using \
                         it — every codec decodes in software. Iterate with \
                         Check capabilities in the application dialog"
                            .to_string(),
                    ),
                    None => (
                        CheckStatus::Warn,
                        ". Whether the browser actually uses it is only \
                         knowable from inside the browser — press Measure now, \
                         or Check capabilities in the application dialog"
                            .to_string(),
                    ),
                };
                detail.push_str(&verdict);
                return self.check(
                    ids::VIDEO_DECODE,
                    "Hardware video decode",
                    status,
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
                format!(
                    "{n} output(s) use a background; swaybg is at {}",
                    path.display()
                ),
            ),
            (n, None) => (
                CheckStatus::Fail,
                format!(
                    "{n} output(s) configure a background but swaybg is not \
                     installed, so sway will accept the command and draw \
                     nothing. Install `swaybg`."
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
            let mtime = std::fs::metadata(&found).and_then(|m| m.modified()).ok();
            match run(&program, &["--version"]).await {
                Ok(output) if output.success => {
                    let version = first_line(&output.stdout);
                    if let Some(mtime) = mtime {
                        self.probed_versions
                            .write()
                            .unwrap()
                            .insert(found.clone(), (mtime, version.clone()));
                    }
                    working.push(format!("{program} ({version})"))
                }
                Ok(output) => broken.push(format!(
                    "{program} exited {}: {}",
                    output.code.unwrap_or(-1),
                    first_line(&output.stderr)
                )),
                // A timeout is a fact about load, not about the binary: the
                // probe races whatever else the machine is doing. If this
                // exact file answered before, believe that answer. An actual
                // failure - wrong exit, missing library - is still reported,
                // because a browser saying "no" is evidence; a busy machine
                // saying nothing is not.
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    let remembered = mtime.and_then(|now| {
                        self.probed_versions
                            .read()
                            .unwrap()
                            .get(&found)
                            .filter(|(seen, _)| *seen == now)
                            .map(|(_, version)| version.clone())
                    });
                    match remembered {
                        Some(version) => working.push(format!("{program} ({version})")),
                        None => broken.push(format!("{program} could not be run: {error}")),
                    }
                }
                Err(error) => broken.push(format!("{program} could not be run: {error}")),
            }
        }

        // What is installed in general matters less than whether the apps
        // this machine is actually configured to run can start. A box with
        // Chromium and a Firefox app is not "fine": that app can never
        // launch, and reporting a pass because *a* browser exists is how
        // that goes unnoticed.
        let mut unrunnable = Vec::new();
        for app in self.store.effective().apps {
            let family = match &app.launcher {
                crate::model::Launcher::ChromiumKiosk { .. } => {
                    crate::supervisor::launcher::CHROMIUM_PROGRAMS
                }
                crate::model::Launcher::FirefoxKiosk { .. } => {
                    crate::supervisor::launcher::FIREFOX_PROGRAMS
                }
                // An `exec` app names its own program; the supervisor reports
                // a missing one as a divergence against that app.
                crate::model::Launcher::Exec { .. } => continue,
            };
            let candidates: Vec<String> = family.iter().map(|p| p.to_string()).collect();
            if crate::supervisor::launcher::resolve_program(&candidates).is_none() {
                unrunnable.push(format!("{} needs {}", app.id, family.join(" or ")));
            }
        }

        // A snap browser is only reached when nothing else is installed, and
        // it will fail in a way that reads as a Chromium bug rather than a
        // packaging one.
        let snapped: Vec<String> = [
            crate::supervisor::launcher::CHROMIUM_PROGRAMS,
            crate::supervisor::launcher::FIREFOX_PROGRAMS,
        ]
        .concat()
        .iter()
        .filter_map(|name| {
            crate::supervisor::launcher::resolve_program(&[name.to_string()])
                .filter(|path| crate::supervisor::launcher::is_snap(path))
                .map(|path| path.display().to_string())
        })
        .collect();
        let only_snap = !snapped.is_empty() && working.is_empty();

        let (status, detail) = if only_snap {
            (
                CheckStatus::Fail,
                format!(
                    concat!(
                        "no browser installed. What is here only launches a snap ({}), ",
                        "and Suede will not use one: a snap updates itself on its own ",
                        "schedule and restarts the browser when it does, which on an ",
                        "appliance means the screens go blank in the middle of a show. ",
                        "Install one from a .deb - on Debian `apt install chromium`, on ",
                        "Ubuntu Google Chrome's own package. To use the snap anyway, ",
                        "name it explicitly: \"program\": \"/snap/bin/chromium\" on the ",
                        "application."
                    ),
                    snapped.join(", ")
                ),
            )
        } else if !unrunnable.is_empty() {
            (
                CheckStatus::Fail,
                format!(
                    "configured apps cannot start: {}. Install the browser, \
                     or change those apps to one that is present{}",
                    unrunnable.join("; "),
                    if working.is_empty() {
                        String::new()
                    } else {
                        format!(" (available: {})", working.join(", "))
                    }
                ),
            )
        } else if working.is_empty() && broken.is_empty() {
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

        // Counting sinks is not enough. When PipeWire can open no devices at
        // all it invents a dummy one, so a machine with no working audio
        // reports the same "1 sink" as a machine with a sound card — the
        // check would pass while nothing could ever be heard.
        let sinks = self.audio.sinks();
        let real = sinks.iter().filter(|sink| !sink.is_null_sink).count();

        let (status, detail) = match (dump_works, pulse_socket, real) {
            // No real output. Which of the three reasons it is decides what
            // the operator should do, so say which rather than listing them.
            (true, true, 0) => match sound_devices() {
                SoundDevices::Denied => (
                    CheckStatus::Warn,
                    "no audio output: the sound devices exist but this user \
                     cannot open them, so PipeWire fell back to a dummy sink. \
                     An appliance has no seated login, so the ACLs that \
                     normally grant access are never applied and group \
                     membership is what counts. Run: sudo usermod -aG audio \
                     $USER, then reboot - a running session keeps the groups \
                     it started with."
                        .to_string(),
                ),
                SoundDevices::Absent => (
                    CheckStatus::Warn,
                    "no audio output: this machine reports no sound devices at \
                     all. Either it has no audio hardware, or no driver is \
                     bound to it - check `lspci -nn | grep -i audio` and \
                     whether the snd_hda_intel module is loaded."
                        .to_string(),
                ),
                SoundDevices::Available => (
                    CheckStatus::Warn,
                    "no audio output: the sound devices are present and \
                     accessible, but no output is active, so PipeWire fell \
                     back to a dummy sink. Analog outputs go inactive when \
                     nothing is plugged into the jacks. Run `wpctl status` to \
                     see the devices and their profiles."
                        .to_string(),
                ),
            },
            (true, true, _) => (
                CheckStatus::Pass,
                format!("{real} audio device(s) available; pipewire-pulse is serving"),
            ),
            (true, false, _) => (
                CheckStatus::Warn,
                "pipewire is running but pipewire-pulse is not; audio routing will not work"
                    .to_string(),
            ),
            (false, _, _) => (
                CheckStatus::Fail,
                "pw-dump is unavailable; audio features are disabled".to_string(),
            ),
        };
        // Restarting the user units cannot conjure a device the user has no
        // permission to open, so do not offer it as the remedy for that.
        let units_would_help = !(dump_works && pulse_socket);

        let mut check = self.check(
            ids::PIPEWIRE,
            "PipeWire audio",
            status,
            detail,
            Some(if status == CheckStatus::Pass {
                "getting-started/#audio"
            } else {
                "troubleshooting/#dummy-output"
            }),
        );
        if check.status != CheckStatus::Pass && units_would_help {
            check.fix_available = true;
            check.fix_description = Some(
                "Start the PipeWire user units (pipewire, wireplumber, \
                 pipewire-pulse). These belong to the session user, so no \
                 privileges are needed."
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

    /// Whether this user can open the machine's capture devices.
    ///
    /// A browser reaches a camera or a capture card through `/dev/videoN`
    /// itself, so the page's permission is only half the story: with the
    /// permission granted and the device node unreadable, `getUserMedia`
    /// fails with `NotReadableError` and the device list comes back empty —
    /// which looks exactly like a permission that was refused, and sends
    /// whoever is debugging it to the wrong end of the problem entirely.
    ///
    /// The test is an open, not a look at the group list: what matters is
    /// whether this process can open the node, and only `PermissionDenied`
    /// answers that. Any other error means the open was allowed and the
    /// device was merely busy — which, when an app is streaming from it, is
    /// the healthy case.
    fn check_capture_devices(&self) -> Check {
        let nodes = capture_nodes();
        if nodes.is_empty() {
            return self.check(
                ids::CAPTURE_DEVICES,
                "Video devices readable",
                CheckStatus::Pass,
                "no video device nodes are present".to_string(),
                None,
            );
        }

        let mut denied: Vec<(PathBuf, u32)> = Vec::new();
        for node in &nodes {
            if let Err(error) = std::fs::File::open(node) {
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    let gid = std::fs::metadata(node)
                        .map(|meta| std::os::unix::fs::MetadataExt::gid(&meta))
                        .unwrap_or(0);
                    denied.push((node.clone(), gid));
                }
            }
        }

        if denied.is_empty() {
            return self.check(
                ids::CAPTURE_DEVICES,
                "Video devices readable",
                CheckStatus::Pass,
                format!(
                    "{} video device node{} present and readable",
                    nodes.len(),
                    if nodes.len() == 1 { "" } else { "s" }
                ),
                None,
            );
        }

        // Name the group that actually owns the node rather than assuming
        // `video`: it is `video` nearly everywhere, and the one machine where
        // it is not is precisely where a guess wastes an afternoon.
        let group = denied
            .iter()
            .find_map(|(_, gid)| group_name(*gid))
            .unwrap_or_else(|| "video".to_string());
        let names: Vec<String> = denied
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();

        self.check(
            ids::CAPTURE_DEVICES,
            "Video devices readable",
            CheckStatus::Warn,
            format!(
                "cannot open {} — a page asking for a camera will get \
                 NotReadableError and an empty device list. Add this user to \
                 the {group} group: `sudo usermod -aG {group} $USER`, then log \
                 out and back in (or reboot); group membership is fixed when \
                 the session starts.",
                names.join(", ")
            ),
            Some("troubleshooting/#a-page-cannot-reach-a-camera-or-capture-device"),
        )
    }

    /// Codec families the last measurement saw hardware-decode; `None` when
    /// nothing has been successfully measured yet.
    fn measured_hardware(&self) -> Option<Vec<String>> {
        let report = self.capabilities.latest()?.report?;
        let mut families: Vec<String> = Vec::new();
        for codec in &report.codecs {
            if codec.hardware == Some(true) {
                let family = codec
                    .label
                    .split_whitespace()
                    .next()
                    .unwrap_or("?")
                    .to_string();
                if !families.contains(&family) {
                    families.push(family);
                }
            }
        }
        Some(families)
    }

    /// What the browser measured about itself, judged.
    ///
    /// Every other check inspects from outside; this one reads the browser's
    /// own report — taken at startup when the configuration changed, or from
    /// the application dialog's button — and says whether it is the report
    /// this hardware should be giving. It never launches anything itself:
    /// checks re-run on a schedule and the measurement opens a window on the
    /// displays, which is a thing a schedule must never do.
    fn check_decode_measured(&self) -> Check {
        let (status, detail) = match self.capabilities.latest() {
            None => (
                CheckStatus::Pass,
                "not measured yet: measured at startup once a browser \
                 application is configured, or from the application dialog"
                    .to_string(),
            ),
            Some(measurement) => match &measurement.report {
                Some(report) => judge_report(report),
                None => (
                    CheckStatus::Warn,
                    format!(
                        "the last capability measurement failed: {}",
                        measurement.note.as_deref().unwrap_or("no reason recorded")
                    ),
                ),
            },
        };
        self.check(
            ids::DECODE_MEASURED,
            "Measured decode capabilities",
            status,
            detail,
            Some("configuration/#environment-and-hardware-acceleration"),
        )
    }

    /// Run the remediation for `id`, returning what was done.
    pub async fn fix(&self, id: &str) -> ApiResult<String> {
        let outcome = match id {
            ids::SYSTEMD_UNIT => self.fix_systemd_unit().await?,
            ids::SWAY_CONFIG => self.fix_sway_config()?,
            ids::PIPEWIRE => self.fix_pipewire().await?,
            ids::DIRECT_SCANOUT => self.fix_direct_scanout().await?,
            ids::OUTPUT_PHASE => self.fix_output_phase().await?,
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

    /// Make the compositor's `WLR_SCENE_DISABLE_DIRECT_SCANOUT` match what
    /// `allow_overlaps` and `direct_scanout` ask for: remove it where the
    /// slicer owns every display and scanout is wanted, set it everywhere
    /// else.
    ///
    /// The variable has to be in the *compositor's* environment, not Suede's,
    /// so the only thing Suede can do without privileges is write (or delete)
    /// a systemd drop-in and ask for a restart. It never restarts sway
    /// itself: that would tear down every window on every display.
    async fn fix_direct_scanout(&self) -> ApiResult<String> {
        let scanout_expected = self.bootstrap.scanout_expected();
        let Some(unit) = compositor_unit() else {
            // No unit is the normal case on an appliance provisioned by
            // provision.sh, where sway is started from the login profile on
            // tty1. There the profile block derives this variable from
            // suede.toml itself, so the keys are already the answer and the
            // only missing step is a session that has read them again.
            return Err(ApiError::Validation(format!(
                "the compositor is not running as a systemd user unit, so Suede cannot \
                 {} its environment. Where provision.sh starts sway from the login \
                 profile on tty1, that profile block derives \
                 WLR_SCENE_DISABLE_DIRECT_SCANOUT from allow_overlaps and direct_scanout \
                 in ~/.config/suede/suede.toml every time it runs: restart the session \
                 so the profile block re-reads suede.toml (log out of tty1, or reboot). \
                 If sway is started some other way, {} there.",
                if scanout_expected { "change" } else { "set" },
                if scanout_expected {
                    "remove the variable"
                } else {
                    "set WLR_SCENE_DISABLE_DIRECT_SCANOUT=1"
                }
            )));
        };

        let dir = self.bootstrap.systemd_user_dir.join(format!("{unit}.d"));
        let path = dir.join("10-suede-scanout.conf");

        if scanout_expected {
            // Only the drop-in Suede itself writes is removed. An operator who
            // set the variable somewhere else — the unit, the profile — is
            // told where to look rather than having their file deleted.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ApiError::Validation(format!(
                        "{} does not exist, so the variable is being set somewhere else \
                         — check {unit} itself and the shell profile that starts sway.",
                        path.display()
                    )));
                }
                Err(error) => {
                    return Err(ApiError::Internal(format!(
                        "cannot remove {}: {error}",
                        path.display()
                    )));
                }
            }

            run("systemctl", &["--user", "daemon-reload"])
                .await
                .map_err(|error| ApiError::Internal(format!("daemon-reload failed: {error}")))?;

            return Ok(format!(
                "removed {} for {unit}. Restart the compositor to apply it \
                 (`systemctl --user restart {unit}`) — Suede will not do that itself, \
                 because it would tear down every window on every display.",
                path.display()
            ));
        }

        std::fs::create_dir_all(&dir).map_err(|error| {
            ApiError::Internal(format!("cannot create {}: {error}", dir.display()))
        })?;

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

    /// Disable every active, real display, wait for sway to actually tear
    /// them down, then re-enable and fully reconfigure them together — see
    /// [`CheckRunner::check_output_phase`] for the measurement this answers.
    ///
    /// A bare `enable` for each output, followed by an ordinary
    /// reconciliation pass to restore mode/position/etc, would *usually*
    /// land in phase too: `plan_outputs`'s diff only reissues a field that
    /// no longer matches. But nothing guarantees every field on every
    /// output needs reissuing after the same disable/enable cycle, and a
    /// *partial* re-apply — some outputs' modes resent in a second, later
    /// IPC message, others left alone because they happened to already
    /// match — reintroduces exactly the one-commit-per-output signature
    /// this fix exists to undo. So this builds the whole plan for these
    /// outputs itself, with an empty `previously_applied` map (which is
    /// `plan_outputs`'s "never seen this output before" case: every field
    /// is issued unconditionally, not only the ones that differ), and sends
    /// it as the one IPC message that carries the `enable`. The trailing
    /// reconcile is then a no-op for anything this touched — observed
    /// state already matches what it would apply — and exists only to let
    /// the ordinary pass record what happened and place windows.
    async fn fix_output_phase(&self) -> ApiResult<String> {
        let active: Vec<Output> = self
            .snapshot
            .outputs()
            .into_iter()
            .filter(|output| output.active && !is_synthetic_output(&output.name))
            .collect();
        if active.len() < 2 {
            return Err(ApiError::Validation(
                "fewer than two active displays; there is nothing to re-align".into(),
            ));
        }

        let disable: Vec<String> = active
            .iter()
            .map(|output| format!("output {} disable", output.name))
            .collect();
        let disable_results = self.sway.run_commands(&disable).await;
        let mut failures = command_failures(&disable, disable_results);
        if failures.len() == disable.len() {
            return Err(ApiError::Internal(format!(
                "could not disable any display: {}",
                failures.join("; ")
            )));
        }

        // Give sway time to actually tear the outputs down — matching the
        // rig's disable/enable cycle above, not the same pass's `enable`,
        // which must land in the very next IPC message rather than a
        // separate one.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let capabilities = Capabilities {
            supports_tearing: self
                .sway
                .get_version()
                .await
                .map(|version| version.supports_tearing())
                .unwrap_or(false),
        };
        let desired: Vec<OutputConfig> = self
            .store
            .effective()
            .outputs
            .into_iter()
            .filter(|config| active.iter().any(|output| config.r#match.matches(output)))
            .collect();

        // `plan_outputs` reads whether an output is active straight off
        // `observed`, and issues `enable` only when it is not — so what is
        // handed in here has to say "disabled", not what the snapshot said
        // before this function touched anything.
        let disabled: Vec<Output> = active
            .iter()
            .cloned()
            .map(|mut output| {
                output.active = false;
                output.current_mode = None;
                output.modes = Vec::new();
                output.rect = Default::default();
                output
            })
            .collect();
        let plan = plan_outputs(&disabled, &desired, &HashMap::new(), capabilities);
        let mut enable: Vec<String> = plan.commands;
        // An output Suede has no configuration entry for (active anyway —
        // sway auto-arranges anything it is not told to leave alone) is not
        // in `desired`, so `plan_outputs` never mentions it. It must still
        // come back: a bare `enable`, appended to the same batch, is the
        // best this fix can do for a display it has no settings opinion on.
        for output in &active {
            if !desired.iter().any(|config| config.r#match.matches(output)) {
                enable.push(format!("output {} enable", output.name));
            }
        }
        let enable_results = self.sway.run_commands(&enable).await;
        failures.extend(command_failures(&enable, enable_results));

        // The reconciler re-derives everything downstream of these outputs
        // (window placement, its own applied-settings cache) on its own
        // schedule; asking here just avoids waiting for the next trigger.
        self.trigger.request("output-phase fix");

        let names: Vec<&str> = active.iter().map(|output| output.name.as_str()).collect();
        let mut outcome = format!(
            "disabled and re-enabled {} together, in one sway command each; \
             the check reflects the result on the next slicer interval (10 s)",
            names.join(", ")
        );
        if !failures.is_empty() {
            outcome.push_str(&format!(
                ". {} command(s) failed: {}",
                failures.len(),
                failures.join("; ")
            ));
        }
        Ok(outcome)
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

/// Pair each command with its result, keeping only the ones that failed, as
/// `"{command}: {error}"`.
fn command_failures(commands: &[String], results: Vec<SwayResult<()>>) -> Vec<String> {
    commands
        .iter()
        .zip(results)
        .filter_map(|(command, result)| result.err().map(|error| format!("{command}: {error}")))
        .collect()
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

/// The compositor setting the two `suede.toml` keys ask for, and the keys
/// themselves — carried together so the check's detail can name what
/// produced the expectation instead of asserting it out of nowhere.
///
/// Derived in exactly one place, [`crate::config::BootstrapConfig::scanout_expected`],
/// so this type and `provision.sh`'s login-time derivation cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanoutExpectation {
    allow_overlaps: bool,
    direct_scanout: bool,
}

impl ScanoutExpectation {
    fn of(bootstrap: &BootstrapConfig) -> Self {
        Self {
            allow_overlaps: bootstrap.allow_overlaps,
            direct_scanout: bootstrap.direct_scanout,
        }
    }

    /// Whether the compositor should have been started *without*
    /// `WLR_SCENE_DISABLE_DIRECT_SCANOUT`.
    fn scanout_expected(self) -> bool {
        self.allow_overlaps && self.direct_scanout
    }

    /// The keys, as they would be written in the file. `direct_scanout` is
    /// named only where it means something: while `allow_overlaps` is false
    /// it cannot be true (the daemon refuses to start on that pair), so
    /// printing its default beside a tiling appliance would read as a
    /// contradiction.
    fn keys(self) -> String {
        if self.allow_overlaps {
            format!(
                "allow_overlaps = true, direct_scanout = {}",
                self.direct_scanout
            )
        } else {
            "allow_overlaps = false".to_string()
        }
    }

    /// One clause naming the keys and what they ask of the compositor, which
    /// every detail string below opens with.
    fn expects(self) -> String {
        format!(
            "{}: the compositor should run with direct scanout {}",
            self.keys(),
            if self.scanout_expected() {
                "enabled"
            } else {
                "disabled"
            }
        )
    }

    fn title(self) -> &'static str {
        match (self.allow_overlaps, self.direct_scanout) {
            (false, _) => "Spanning renders across outputs",
            (true, true) => "Projectors scan out directly",
            (true, false) => "Direct scanout is off by request",
        }
    }
}

/// What the `direct-scanout` check should be saying, given what the two keys
/// ask for, which apps span, and whether the compositor was started with
/// `WLR_SCENE_DISABLE_DIRECT_SCANOUT`. Pure, so the whole matrix is
/// table-testable.
///
/// The modes want opposite things from the same variable — see
/// [`CheckRunner::check_direct_scanout`] for why.
fn judge_direct_scanout(
    expectation: ScanoutExpectation,
    spanning: &[String],
    disabled: bool,
) -> (CheckStatus, String) {
    if expectation.allow_overlaps {
        // Nothing spans the physical outputs here, so which apps are enabled
        // does not enter into it: the slicer gives every display its own
        // buffer either way, and the only question is whether the compositor
        // is allowed to flip it.
        return match (expectation.scanout_expected(), disabled) {
            (true, true) => (
                CheckStatus::Warn,
                format!(
                    "{}, but it was started with WLR_SCENE_DISABLE_DIRECT_SCANOUT set. \
                     This appliance slices every layout, so no window ever spans two \
                     displays and the mirroring bug that variable works around cannot \
                     happen. Every projector pays a compositor pass for nothing; remove \
                     the drop-in and restart the compositor.",
                    expectation.expects()
                ),
            ),
            (true, false) => (
                CheckStatus::Pass,
                format!(
                    "{}: direct scanout is enabled, so each projector's slice can be \
                     flipped straight to the display",
                    expectation.keys()
                ),
            ),
            (false, true) => (
                CheckStatus::Pass,
                format!(
                    "{}: WLR_SCENE_DISABLE_DIRECT_SCANOUT is set, so sway composites \
                     every slice — the comparison arm this machine was asked for",
                    expectation.keys()
                ),
            ),
            (false, false) => (
                CheckStatus::Warn,
                format!(
                    "{}, but WLR_SCENE_DISABLE_DIRECT_SCANOUT is not set, so the \
                     compositor is still flipping each slice straight to the display \
                     and the comparison is not in effect. Set the variable and restart \
                     the compositor, or remove direct_scanout from suede.toml.",
                    expectation.expects()
                ),
            ),
        };
    }

    match (spanning.is_empty(), disabled) {
        (_, true) => (
            CheckStatus::Pass,
            format!(
                "{}: direct scanout is disabled, so a spanned window covers every output",
                expectation.keys()
            ),
        ),
        (true, false) => (
            CheckStatus::Pass,
            format!(
                "{}: no app spans outputs, so direct scanout is harmless",
                expectation.keys()
            ),
        ),
        (false, false) => (
            CheckStatus::Warn,
            format!(
                "{}, but {} spans every output and the compositor was started with \
                 direct scanout enabled. On some drivers (notably Nvidia's) this makes \
                 each display show the same part of the window instead of its own — the \
                 window is the right size, it simply renders wrong. Start sway with \
                 WLR_SCENE_DISABLE_DIRECT_SCANOUT=1.",
                expectation.expects(),
                spanning.join(", ")
            ),
        ),
    }
}

/// Judge a browser's own capability report against what this hardware
/// should be saying. Pure, so every verdict is table-testable.
///
/// The rules are the ones field measurements taught:
/// - a software rasteriser in the renderer string means no GPU acceleration
///   of any kind, which makes every other answer moot — warn;
/// - a real GPU with zero hardware codecs is the classic silent fallback
///   (an NVIDIA machine without `VaapiOnNvidiaGPUs` looked exactly like
///   this) — warn, unless the platform genuinely has no browser decode
///   path, which VideoCore does not: on a Pi that state is the truth, and
///   warning about physics forever only teaches people to ignore the check.
fn judge_report(report: &crate::model::CapabilityReport) -> (CheckStatus, String) {
    let renderer = report.gpu_renderer.as_deref().unwrap_or("unknown");
    let lowered = renderer.to_ascii_lowercase();

    if ["llvmpipe", "swiftshader", "softpipe"]
        .iter()
        .any(|soft| lowered.contains(soft))
    {
        return (
            CheckStatus::Warn,
            format!(
                "the browser is software-rendering ({renderer}): no GPU \
                 acceleration of any kind — decode, rasterisation or WebGL"
            ),
        );
    }

    // Codec families, first word of the label, order preserved.
    let mut hardware: Vec<&str> = Vec::new();
    let mut software: Vec<&str> = Vec::new();
    for codec in &report.codecs {
        let family = codec.label.split_whitespace().next().unwrap_or("?");
        let list = match codec.hardware {
            Some(true) => &mut hardware,
            _ if codec.supported => &mut software,
            _ => continue,
        };
        if !list.contains(&family) {
            list.push(family);
        }
    }
    software.retain(|family| !hardware.contains(family));

    if hardware.is_empty() {
        let expected_platform = ["videocore", "broadcom", "v3d"]
            .iter()
            .any(|pi| lowered.contains(pi));
        if expected_platform {
            return (
                CheckStatus::Pass,
                format!(
                    "every codec decodes in software on {renderer}, which is \
                     expected: no browser reaches VideoCore's decoder yet. \
                     Prefer H.264 or VP9 at 1080p for this machine"
                ),
            );
        }
        return (
            CheckStatus::Warn,
            format!(
                "a GPU is present ({renderer}) but every codec decodes in \
                 software — the silent fallback that looks fine until the \
                 content is demanding. Re-check from the application dialog \
                 after changing arguments or drivers"
            ),
        );
    }

    let mut detail = format!("hardware decode: {}", hardware.join(", "));
    if !software.is_empty() {
        detail.push_str(&format!("; software only: {}", software.join(", ")));
    }
    detail.push_str(&format!(" — measured on {renderer}"));
    (CheckStatus::Pass, detail)
}

/// Outputs invented by the compositor rather than driven from a connector.
pub fn is_synthetic_output(name: &str) -> bool {
    name.starts_with("HEADLESS-") || name.starts_with("WL-") || name.starts_with("X11-")
}

/// Judge whether the active, physical displays are running at the same
/// refresh rate.
///
/// Pure, so every combination is table-testable without a compositor. Found
/// because a camera filming two projectors showed their frame counters off
/// by one most of the time: they were mode-set at 60.000 Hz and 59.939 Hz —
/// both advertised both — which drifts a whole frame apart roughly every 16
/// seconds. Nothing warned, because each output was individually healthy.
///
/// Rates are grouped to 0.001 Hz: tight enough to never conflate genuinely
/// different rates (50/60/72/75), loose enough that this does not itself
/// manufacture a mismatch out of floating-point noise in the same rate.
fn refresh_rate_verdict(outputs: &[Output], configured: &[OutputConfig]) -> (CheckStatus, String) {
    let active: Vec<&Output> = outputs
        .iter()
        .filter(|output| {
            output.active && !is_synthetic_output(&output.name) && output.current_mode.is_some()
        })
        .collect();

    if active.len() < 2 {
        return match active.first() {
            None => (CheckStatus::Pass, "no displays attached".to_string()),
            Some(output) => (
                CheckStatus::Pass,
                format!(
                    "one display attached ({} at {} Hz); nothing to keep in step",
                    output.name,
                    format_refresh(output.current_mode.unwrap().refresh_hz)
                ),
            ),
        };
    }

    let key = |hz: f64| (hz * 1000.0).round() as i64;

    // Groups, in first-seen order — order matters below, both for the
    // sentence naming the outputs and for which common rate is offered as
    // the remedy when more than one would do.
    let mut groups: Vec<(i64, Vec<&Output>)> = Vec::new();
    for output in &active {
        let rate_key = key(output.current_mode.unwrap().refresh_hz);
        match groups.iter_mut().find(|(k, _)| *k == rate_key) {
            Some((_, members)) => members.push(output),
            None => groups.push((rate_key, vec![output])),
        }
    }

    if groups.len() == 1 {
        let rate = groups[0].0 as f64 / 1000.0;
        let names: Vec<&str> = active.iter().map(|output| output.name.as_str()).collect();
        return (
            CheckStatus::Pass,
            format!(
                "{} all run at {} Hz",
                names.join(", "),
                format_refresh(rate)
            ),
        );
    }

    // Name every output with its rate: "DP-1 runs at 60 Hz but DP-3 runs at
    // 59.939 Hz" for two, "A, B run at 60 Hz but C runs at 50 Hz" grouped by
    // rate when more share one.
    let phrase_for = |members: &[&Output]| -> String {
        let names: Vec<&str> = members.iter().map(|output| output.name.as_str()).collect();
        let rate = format_refresh(members[0].current_mode.unwrap().refresh_hz);
        if names.len() == 1 {
            format!("{} runs at {rate} Hz", names[0])
        } else {
            format!("{} run at {rate} Hz", names.join(", "))
        }
    };
    let mut phrases: Vec<String> = groups
        .iter()
        .map(|(_, members)| phrase_for(members))
        .collect();
    let naming = if phrases.len() == 1 {
        phrases.remove(0)
    } else {
        let last = phrases.pop().expect("more than one group");
        format!("{} but {last}", phrases.join(", "))
    };

    // The drift period: how long before the two extremes are a whole frame
    // apart. `1 / |Δ Hz|` — 60 vs 59.939 drifts a frame every ~16.4 s.
    let rates: Vec<f64> = groups.iter().map(|(k, _)| *k as f64 / 1000.0).collect();
    let min_rate = rates.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_rate = rates.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let drift_seconds = 1.0 / (max_rate - min_rate).abs();
    let consequence = format!(
        "At these rates they drift a whole frame apart every {} s, so a camera \
         catches the displays a frame out of step and one of them repeats a \
         frame each cycle",
        drift_seconds.round() as i64
    );

    // A remedy: is there a rate, advertised at each output's own current
    // resolution, that every mismatched output offers? Candidates are the
    // rates already in use (tried in first-seen order, so a tie prefers not
    // disturbing the output that already leads), because the point is to
    // change as little as possible, not to invent a new rate nobody is at.
    let advertises = |output: &Output, rate_key: i64| {
        let current = output.current_mode.unwrap();
        output.modes.iter().any(|mode| {
            mode.width == current.width
                && mode.height == current.height
                && key(mode.refresh_hz) == rate_key
        })
    };
    let common = groups
        .iter()
        .find(|(rate_key, _)| active.iter().all(|output| advertises(output, *rate_key)))
        .map(|(rate_key, _)| *rate_key as f64 / 1000.0);

    let remedy = match common {
        Some(rate) => {
            let rate_key = key(rate);
            let already: Vec<&str> = groups
                .iter()
                .find(|(k, _)| *k == rate_key)
                .map(|(_, members)| members.iter().map(|output| output.name.as_str()).collect())
                .unwrap_or_default();
            let to_change: Vec<&str> = active
                .iter()
                .map(|output| output.name.as_str())
                .filter(|name| !already.contains(name))
                .collect();
            format!(
                "All of them advertise {} Hz at their current resolution — set that on {} \
                 in the layout editor",
                format_refresh(rate),
                to_change.join(", ")
            )
        }
        None => "They advertise no common rate at their current resolutions; pick the nearest pair"
            .to_string(),
    };

    let mut detail = format!("{naming}. {consequence}. {remedy}.");

    // If the configuration itself asked for these differing rates — rather
    // than sway simply defaulting each output to its own preferred mode —
    // say so: this was requested, not merely undetected drift.
    let mut explicit_rate_keys: Vec<i64> = Vec::new();
    for output in &active {
        let Some(entry) = configured
            .iter()
            .find(|entry| entry.r#match.name.as_deref() == Some(output.name.as_str()))
        else {
            continue;
        };
        if let Some(mode) = entry.mode {
            let rate_key = key(mode.refresh_hz);
            if !explicit_rate_keys.contains(&rate_key) {
                explicit_rate_keys.push(rate_key);
            }
        }
    }
    if explicit_rate_keys.len() > 1 {
        detail.push_str(" The configuration asks for these rates.");
    }

    (CheckStatus::Warn, detail)
}

/// Judge whether the active displays' vblank phase is aligned, from the
/// slicer's own presentation-timestamp measurement.
///
/// Pure, so every combination is table-testable without a compositor or a
/// slicer. See [`CheckRunner::check_output_phase`] for the measurement
/// behind the 1.0 ms threshold and what a batched re-enable does about it.
///
/// `running` is `Snapshot::slicer_running`, not inferred from `stats`: a
/// four-projector bench on 2026-09-15 had a slicer that was alive and had
/// logged its startup line, but had produced no frames because the frame
/// loop is damage-driven and the active page was static — `stats` was
/// `None` exactly as it would be with no slicer at all, and this check said
/// "not measured: the slicer is not running" about a slicer that was
/// running. The two cases now get their own, both true, sentences.
fn output_phase_verdict(running: bool, stats: Option<&ProjectionStats>) -> (CheckStatus, String) {
    let Some(stats) = stats else {
        return if running {
            (
                CheckStatus::Pass,
                "not measured: the slicer has reported no frames yet, which is what a \
                 static page looks like — the frame loop only runs when the content draws"
                    .to_string(),
            )
        } else {
            (
                CheckStatus::Pass,
                "not measured: no slicer is running".to_string(),
            )
        };
    };
    if !stats.presentation_feedback {
        return (
            CheckStatus::Pass,
            "the compositor offers no presentation timing, so phase cannot be measured".to_string(),
        );
    }

    let measured: Vec<&OutputTiming> = stats
        .outputs
        .iter()
        .filter(|output| output.phase_ms.is_some())
        .collect();
    if measured.len() < 2 {
        return (
            CheckStatus::Pass,
            "fewer than two displays report a phase; nothing to compare".to_string(),
        );
    }

    // Index 0 is the slicer's own reference: its phase is 0.0 by
    // definition, against itself. Anything else more than 1.0 ms away from
    // that is out of phase with it.
    let reference = measured[0];
    let out_of_phase: Vec<&OutputTiming> = measured[1..]
        .iter()
        .filter(|output| output.phase_ms.unwrap().abs() > 1.0)
        .copied()
        .collect();

    if out_of_phase.is_empty() {
        let max_abs = measured
            .iter()
            .map(|output| output.phase_ms.unwrap().abs())
            .fold(0.0_f64, f64::max);
        let names: Vec<&str> = measured.iter().map(|output| output.name.as_str()).collect();
        return (
            CheckStatus::Pass,
            format!("{} within {max_abs:.2} ms", names.join(", ")),
        );
    }

    let consequence = "Heads that were enabled one at a time start their rasters at \
        different moments; the same frame then lands on different refreshes for a \
        quarter of the time. Re-enabling every output together puts them in phase.";

    // The common case (this is the rig's own signature): every other head
    // agrees with itself but not with the reference, which makes the
    // *reference* the odd one out even though its own recorded phase is
    // 0.0 by construction. Naming the group it disagrees with, and the
    // size of that disagreement, is what an operator standing at the rack
    // needs — not which output the measurement happened to be taken from.
    let in_phase_with_reference = measured[1..]
        .iter()
        .any(|output| output.phase_ms.unwrap().abs() <= 1.0);
    if !in_phase_with_reference {
        let values: Vec<f64> = out_of_phase
            .iter()
            .map(|output| output.phase_ms.unwrap())
            .collect();
        let spread = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - values.iter().cloned().fold(f64::INFINITY, f64::min);
        if spread <= 1.0 {
            let names: Vec<&str> = out_of_phase
                .iter()
                .map(|output| output.name.as_str())
                .collect();
            return (
                CheckStatus::Warn,
                format!(
                    "{} is {:.1} ms out of phase with {} (measured from presentation \
                     timestamps). {consequence}",
                    reference.name,
                    values[0],
                    names.join(", ")
                ),
            );
        }
    }

    // The general case: whichever outputs disagree with the reference, each
    // named with its own measured offset.
    let parts: Vec<String> = out_of_phase
        .iter()
        .map(|output| format!("{} {:+.1} ms", output.name, output.phase_ms.unwrap()))
        .collect();
    (
        CheckStatus::Warn,
        format!(
            "out of phase with {}: {} (measured from presentation timestamps). {consequence}",
            reference.name,
            parts.join(", ")
        ),
    )
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

/// Decode status for a Broadcom VideoCore GPU, or `None` on other hardware.
///
/// Identified from the driver bound to the DRM device rather than a vendor
/// ID, because platform devices have none: `v3d` renders and `vc4` scans out,
/// and either one means a Raspberry Pi (or close relative). What decode
/// hardware the model actually has is read from the V4L2 nodes — a Pi 5
/// exposes `rpi-hevc-dec` (stateless, HEVC only; H.264 lost its hardware
/// path with the 2712), a Pi 4 `bcm2835-codec-decode` (stateful, H.264
/// included). Verified on a Pi 5 Model B running Raspberry Pi OS Trixie.
fn videocore_decode() -> Option<(CheckStatus, String)> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    let videocore = entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Cards only: `card0`, not connectors like `card0-HDMI-A-1`.
        if !name.starts_with("card") || name.contains('-') {
            return false;
        }
        let Ok(uevent) = std::fs::read_to_string(entry.path().join("device/uevent")) else {
            return false;
        };
        uevent.lines().any(|line| {
            matches!(
                line.strip_prefix("DRIVER="),
                Some("v3d" | "vc4" | "vc4-drm")
            )
        })
    });
    if !videocore {
        return None;
    }

    let mut decoders: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") {
        for entry in entries.flatten() {
            let Ok(name) = std::fs::read_to_string(entry.path().join("name")) else {
                continue;
            };
            let name = name.trim();
            // Decoders only; the same SoC also exposes ISP and encoder nodes.
            if name.contains("dec") && !decoders.iter().any(|d| d == name) {
                decoders.push(name.to_string());
            }
        }
    }
    decoders.sort();

    if decoders.is_empty() {
        return Some((
            CheckStatus::Warn,
            "Broadcom VideoCore GPU with no V4L2 decoder exposed — video will \
             decode on the CPU"
                .to_string(),
        ));
    }
    let mut detail = format!(
        "Broadcom VideoCore: decode here is V4L2 ({}), not VA-API, and \
         Raspberry Pi OS's Chromium uses it directly",
        decoders.join(", ")
    );
    if decoders.iter().all(|d| d.contains("hevc")) {
        detail.push_str(
            ". H.264 has no hardware path on this model and decodes in \
             software — fine at 1080p, marginal above it",
        );
    }
    Some((CheckStatus::Pass, detail))
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

/// Every `/dev/videoN` the kernel is offering, sorted.
///
/// Not all of them are cameras, and the count is deliberately not described as
/// though they were: a UVC device presents a capture node *and* a metadata
/// node, and a Raspberry Pi 5 presents seventeen with no camera attached at
/// all — one HEVC decoder and sixteen ISP nodes. Telling them apart needs a
/// V4L2 ioctl, which would cost a libc dependency for no gain here, because
/// every one of them is opened by something and every one can carry the wrong
/// permissions on its own. So all are checked, and the message says nodes.
fn capture_nodes() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("video"))
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    nodes.sort();
    nodes
}

/// Group name for a gid, from `/etc/group`.
fn group_name(gid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/group").ok()?;
    group_name_in(&text, gid)
}

fn group_name_in(text: &str, gid: u32) -> Option<String> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        (fields.next()?.parse::<u32>().ok()? == gid).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::MockAudio;
    use crate::model::LagFrames;
    use crate::sway::mock::MockSway;

    #[test]
    fn no_sound_nodes_at_all_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            sound_devices_in(dir.path().to_str().unwrap()),
            SoundDevices::Absent
        );
        // A directory that is not there at all is the same answer, not a panic.
        assert_eq!(
            sound_devices_in("/definitely/not/here"),
            SoundDevices::Absent
        );
    }

    #[test]
    fn only_non_control_nodes_still_reads_as_absent() {
        // seq and timer exist on machines with no sound card; neither is a
        // card, so neither should suggest one is present.
        let dir = tempfile::tempdir().unwrap();
        for name in ["seq", "timer"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert_eq!(
            sound_devices_in(dir.path().to_str().unwrap()),
            SoundDevices::Absent
        );
    }

    #[test]
    fn a_readable_control_node_reads_as_available() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("controlC0"), b"").unwrap();
        std::fs::write(dir.path().join("pcmC0D0p"), b"").unwrap();
        assert_eq!(
            sound_devices_in(dir.path().to_str().unwrap()),
            SoundDevices::Available
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unopenable_control_node_reads_as_denied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("controlC0");
        std::fs::write(&node, b"").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode bits, so this can only be asserted as a
        // normal user; skip rather than fail in a root container.
        if std::fs::File::open(&node).is_ok() {
            return;
        }
        assert_eq!(
            sound_devices_in(dir.path().to_str().unwrap()),
            SoundDevices::Denied
        );
    }

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
        let capabilities = Arc::new(crate::capabilities::CapabilityStore::new(
            &bootstrap.state_dir,
        ));
        let (trigger, _receiver) = crate::reconciler::Reconciler::channel();
        CheckRunner::new(
            bootstrap,
            Arc::new(MockSway::with_fixtures()),
            Arc::new(MockAudio::with_devices()),
            store,
            EventHub::new(),
            capabilities,
            Arc::new(Snapshot::new()),
            trigger,
        )
    }

    /// The same runner, on an appliance that slices every layout, with
    /// `direct_scanout` as given — the two bootstrap keys that decide what
    /// the `direct-scanout` check expects of the compositor.
    fn runner_slicing(state_dir: std::path::PathBuf, direct_scanout: bool) -> CheckRunner {
        let runner = runner(state_dir);
        let bootstrap = Arc::new(BootstrapConfig {
            allow_overlaps: true,
            direct_scanout,
            ..(*runner.bootstrap).clone()
        });
        CheckRunner {
            bootstrap,
            ..runner
        }
    }

    /// The expectation a `suede.toml` with these keys produces. `false` for
    /// `allow_overlaps` is the tiling appliance, where `direct_scanout` can
    /// only be its default (the daemon refuses to start on an explicit true).
    fn expectation(allow_overlaps: bool, direct_scanout: bool) -> ScanoutExpectation {
        ScanoutExpectation {
            allow_overlaps,
            direct_scanout,
        }
    }

    #[test]
    fn a_tiling_appliance_wants_direct_scanout_disabled() {
        let tiling = expectation(false, true);
        let spanning = vec!["wall".to_string()];

        // The variable set is the whole point of a tiling appliance, whether
        // or not anything currently spans.
        let (status, detail) = judge_direct_scanout(tiling, &spanning, true);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("disabled"), "{detail}");
        assert_eq!(judge_direct_scanout(tiling, &[], true).0, CheckStatus::Pass);

        // Unset with nothing spanning is harmless: no client covers two
        // displays, so there is nothing to mirror.
        let (status, detail) = judge_direct_scanout(tiling, &[], false);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("no app spans"), "{detail}");

        // Unset with a spanning app is the bug this check was written for,
        // and it names the app.
        let (status, detail) = judge_direct_scanout(tiling, &spanning, false);
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("wall"), "{detail}");
        assert!(
            detail.contains("WLR_SCENE_DISABLE_DIRECT_SCANOUT=1"),
            "{detail}"
        );

        // Only `allow_overlaps` is named: `direct_scanout` has no meaning
        // here, and printing its default would read as a contradiction.
        for detail in [
            judge_direct_scanout(tiling, &spanning, true).1,
            judge_direct_scanout(tiling, &spanning, false).1,
        ] {
            assert!(detail.contains("allow_overlaps = false"), "{detail}");
            assert!(!detail.contains("direct_scanout"), "{detail}");
        }
    }

    #[test]
    fn a_slicing_appliance_wants_direct_scanout_left_alone() {
        // Exactly inverted, and spanning apps do not enter into it: the app
        // spans the headless canvas, never the physical outputs.
        let slicing = expectation(true, true);
        for spanning in [vec![], vec!["wall".to_string()]] {
            let (status, detail) = judge_direct_scanout(slicing, &spanning, true);
            assert_eq!(status, CheckStatus::Warn, "{detail}");
            assert!(detail.contains("compositor pass"), "{detail}");
            assert!(!detail.contains("wall"), "{detail}");

            let (status, detail) = judge_direct_scanout(slicing, &spanning, false);
            assert_eq!(status, CheckStatus::Pass, "{detail}");
        }
    }

    /// `direct_scanout = false` is the other arm of the A/B: the same sliced
    /// layout, composited instead of flipped. The expectation inverts again,
    /// so a machine that has not actually been restarted into it is a warning
    /// rather than a silent comparison of the machine with itself.
    #[test]
    fn the_a_b_switch_inverts_the_expectation_again() {
        let off = expectation(true, false);

        let (status, detail) = judge_direct_scanout(off, &[], true);
        assert_eq!(status, CheckStatus::Pass, "{detail}");
        assert!(detail.contains("composites every slice"), "{detail}");

        let (status, detail) = judge_direct_scanout(off, &[], false);
        assert_eq!(status, CheckStatus::Warn, "{detail}");
        assert!(detail.contains("not in effect"), "{detail}");

        // Every verdict says which pair of keys produced the expectation.
        for (expected, disabled) in [(true, true), (true, false), (false, true), (false, false)] {
            let detail = judge_direct_scanout(expectation(true, expected), &[], disabled).1;
            assert!(
                detail.contains(&format!(
                    "allow_overlaps = true, direct_scanout = {expected}"
                )),
                "{detail}"
            );
        }
    }

    #[test]
    fn the_scanout_check_points_each_mode_at_its_own_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let tiling = runner(dir.path().to_path_buf()).check_direct_scanout();
        assert_eq!(tiling.title, "Spanning renders across outputs");
        assert!(
            tiling
                .docs_url
                .as_deref()
                .is_some_and(|url| url.ends_with("#a-spanned-window-mirrors-instead-of-spanning")),
            "{:?}",
            tiling.docs_url
        );

        let slicing = runner_slicing(dir.path().to_path_buf(), true).check_direct_scanout();
        assert_eq!(slicing.title, "Projectors scan out directly");
        assert!(
            slicing
                .docs_url
                .as_deref()
                .is_some_and(|url| url.ends_with("#direct-scanout")),
            "{:?}",
            slicing.docs_url
        );

        // The A/B's other arm is the same page, under a title that does not
        // claim the projectors are scanning out when they were asked not to.
        let composited = runner_slicing(dir.path().to_path_buf(), false).check_direct_scanout();
        assert_eq!(composited.title, "Direct scanout is off by request");
        assert!(
            composited
                .docs_url
                .as_deref()
                .is_some_and(|url| url.ends_with("#direct-scanout")),
            "{:?}",
            composited.docs_url
        );
    }

    fn codec(family: &str, supported: bool, hardware: Option<bool>) -> crate::model::CodecSupport {
        crate::model::CodecSupport {
            label: format!("{family} High 1080p60"),
            content_type: "video/mp4".into(),
            supported,
            smooth: Some(supported),
            power_efficient: hardware,
            hardware,
        }
    }

    fn measured(
        renderer: &str,
        codecs: Vec<crate::model::CodecSupport>,
    ) -> crate::model::CapabilityReport {
        crate::model::CapabilityReport {
            user_agent: "test".into(),
            gpu_vendor: None,
            gpu_renderer: Some(renderer.into()),
            webgpu: true,
            video_decoder_api: true,
            notes: vec![],
            codecs,
        }
    }

    #[test]
    fn a_measured_hardware_decoder_passes_and_is_listed() {
        let (status, detail) = judge_report(&measured(
            "NVIDIA RTX A1000",
            vec![
                codec("H.264", true, Some(true)),
                codec("AV1", true, Some(false)),
            ],
        ));
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("hardware decode: H.264"), "{detail}");
        assert!(detail.contains("software only: AV1"), "{detail}");
    }

    #[test]
    fn a_gpu_with_no_hardware_codecs_warns() {
        // The exact state a gated NVIDIA machine was measured in.
        let (status, detail) = judge_report(&measured(
            "ANGLE (NVIDIA, Quadro RTX 8000, OpenGL ES 3.2)",
            vec![codec("H.264", true, Some(false))],
        ));
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("silent fallback"), "{detail}");
    }

    #[test]
    fn software_only_on_videocore_is_expected_not_warned() {
        let (status, detail) = judge_report(&measured(
            "ANGLE (Broadcom, V3D 7.1.7.0, OpenGL ES 3.1)",
            vec![codec("H.264", true, Some(false))],
        ));
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("expected"), "{detail}");
    }

    #[test]
    fn a_software_rasteriser_warns_regardless_of_codecs() {
        let (status, detail) = judge_report(&measured(
            "llvmpipe (LLVM 17.0.6, 256 bits)",
            vec![codec("H.264", true, Some(true))],
        ));
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("software-rendering"), "{detail}");
    }

    #[tokio::test]
    async fn an_unmeasured_machine_passes_with_an_explanation() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let checks = runner.run_all().await;
        let check = checks
            .iter()
            .find(|c| c.id == ids::DECODE_MEASURED)
            .expect("the check runs");
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("not measured"), "{}", check.detail);
    }

    #[tokio::test]
    async fn measured_hardware_reflects_the_stored_report() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        assert!(runner.measured_hardware().is_none(), "nothing measured");

        let app: crate::model::AppConfig = serde_json::from_value(serde_json::json!({
            "id": "renderer",
            "launcher": { "kind": "chromium-kiosk", "uri": "http://x/" },
        }))
        .unwrap();
        let key = crate::capabilities::MeasurementKey::new(&app, std::path::Path::new("/bin/true"));

        // A failed measurement is not a report: the hedge must stay.
        runner
            .capabilities
            .record(crate::capabilities::StoredMeasurement {
                measured_at: 1,
                app_id: "renderer".into(),
                key: key.clone(),
                report: None,
                note: Some("exited".into()),
            });
        assert!(
            runner.measured_hardware().is_none(),
            "a failure proves nothing"
        );

        runner
            .capabilities
            .record(crate::capabilities::StoredMeasurement {
                measured_at: 2,
                app_id: "renderer".into(),
                key,
                report: Some(measured(
                    "NVIDIA",
                    vec![
                        codec("H.264", true, Some(true)),
                        codec("H.265", true, Some(true)),
                        codec("AV1", true, Some(false)),
                    ],
                )),
                note: None,
            });
        assert_eq!(
            runner.measured_hardware().unwrap(),
            vec!["H.264", "H.265"],
            "hardware families only, order preserved"
        );
    }

    #[tokio::test]
    async fn a_failed_measurement_warns_with_its_note() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let app: crate::model::AppConfig = serde_json::from_value(serde_json::json!({
            "id": "renderer",
            "launcher": { "kind": "chromium-kiosk", "uri": "http://x/" },
        }))
        .unwrap();
        runner
            .capabilities
            .record(crate::capabilities::StoredMeasurement {
                measured_at: 1,
                app_id: "renderer".into(),
                key: crate::capabilities::MeasurementKey::new(
                    &app,
                    std::path::Path::new("/bin/true"),
                ),
                report: None,
                note: Some("the browser exited (status 1) before reporting".into()),
            });
        let checks = runner.run_all().await;
        let check = checks
            .iter()
            .find(|c| c.id == ids::DECODE_MEASURED)
            .unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("exited"), "{}", check.detail);
    }

    #[tokio::test]
    async fn every_check_reports_something() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        assert_eq!(checks.len(), ids::ALL.len());
        for id in ids::ALL {
            assert!(checks.iter().any(|check| check.id == *id), "missing {id}");
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
        let (trigger, _receiver) = crate::reconciler::Reconciler::channel();
        let runner = CheckRunner::new(
            bootstrap,
            sway,
            Arc::new(MockAudio::default()),
            Arc::new(crate::state::StateStore::ephemeral(
                dir.path().to_path_buf(),
            )),
            EventHub::new(),
            Arc::new(crate::capabilities::CapabilityStore::new(dir.path())),
            Arc::new(Snapshot::new()),
            trigger,
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
    async fn fix_output_phase_disables_then_reenables_together_and_requests_a_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::with_fixtures());
        let store = Arc::new(crate::state::StateStore::ephemeral(
            dir.path().to_path_buf(),
        ));
        store
            .update(|state| {
                for name in ["HDMI-A-1", "HDMI-A-2"] {
                    let mut config = OutputConfig::new(crate::model::OutputMatch::by_name(name));
                    config.mode = Some(crate::model::Mode {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60.0,
                    });
                    config.position = Some(crate::model::Position { x: 0, y: 0 });
                    state.outputs.push(config);
                }
            })
            .unwrap();
        let snapshot = Arc::new(Snapshot::new());
        snapshot.set_outputs(sway.get_outputs().await.unwrap());
        let (trigger, mut receiver) = crate::reconciler::Reconciler::channel();
        let runner = CheckRunner::new(
            bootstrap,
            sway.clone(),
            Arc::new(MockAudio::default()),
            store,
            EventHub::new(),
            Arc::new(crate::capabilities::CapabilityStore::new(dir.path())),
            snapshot,
            trigger,
        );

        let detail = runner.fix(ids::OUTPUT_PHASE).await.unwrap();
        assert!(detail.contains("HDMI-A-1"), "{detail}");
        assert!(detail.contains("HDMI-A-2"), "{detail}");
        assert!(detail.contains("10 s"), "{detail}");

        let commands = sway.commands();
        assert!(
            commands.iter().any(|c| c == "output HDMI-A-1 disable"),
            "{commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == "output HDMI-A-2 disable"),
            "{commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == "output HDMI-A-1 enable"),
            "{commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == "output HDMI-A-2 enable"),
            "{commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("mode 1920x1080@60Hz")),
            "the mode must be reapplied in the same pass as the enable: {commands:?}"
        );

        let outputs = sway.get_outputs().await.unwrap();
        assert!(
            outputs
                .iter()
                .filter(|o| o.name == "HDMI-A-1" || o.name == "HDMI-A-2")
                .all(|o| o.active),
            "both re-aligned outputs must come back up: {outputs:?}"
        );

        assert!(
            receiver.try_recv().is_ok(),
            "a reconcile must be requested so downstream state catches up"
        );
    }

    #[tokio::test]
    async fn fix_output_phase_needs_at_least_two_active_displays() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::empty());
        let snapshot = Arc::new(Snapshot::new());
        snapshot.set_outputs(vec![output_running_at("DP-1", 60.0, &[])]);
        let (trigger, _receiver) = crate::reconciler::Reconciler::channel();
        let runner = CheckRunner::new(
            bootstrap,
            sway,
            Arc::new(MockAudio::default()),
            Arc::new(crate::state::StateStore::ephemeral(
                dir.path().to_path_buf(),
            )),
            EventHub::new(),
            Arc::new(crate::capabilities::CapabilityStore::new(dir.path())),
            snapshot,
            trigger,
        );

        let error = runner.fix(ids::OUTPUT_PHASE).await.unwrap_err();
        assert!(matches!(error, ApiError::Validation(_)));
    }

    #[tokio::test]
    async fn checks_publish_only_when_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let mut receiver = runner.events.subscribe();

        runner.run_all().await;
        assert!(receiver.try_recv().is_ok(), "first run should publish");

        // One settling run before demanding silence. The first pass probes
        // real programs on a possibly loaded machine, and a probe that raced
        // the load may answer differently once - CI hit exactly that, with
        // chromium timing out on the first run and answering on the second.
        // That flip is a legitimate publish; what may never happen is a
        // THIRD answer, because from a settled state an unchanged machine
        // must stay quiet.
        let first = runner.run_all().await;
        while receiver.try_recv().is_ok() {}

        let second = runner.run_all().await;
        if receiver.try_recv().is_ok() {
            // Naming the culprit, because "something changed" is not a lead.
            // A check that answers differently on two consecutive runs would
            // also republish forever on a real appliance, so this failing is
            // a genuine finding rather than a flaky test to be retried.
            let mut moved = Vec::new();
            for (before, after) in first.iter().zip(second.iter()) {
                if before != after {
                    moved.push(format!(
                        "{}: {:?}/{:?} -> {:?}/{:?}",
                        after.id, before.status, before.detail, after.status, after.detail
                    ));
                }
            }
            if first.len() != second.len() {
                moved.push(format!(
                    "the number of checks changed: {} -> {}",
                    first.len(),
                    second.len()
                ));
            }
            panic!("an unchanged second run should stay quiet; these moved: {moved:#?}");
        }
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

    /// A 1920x1080 output currently at `current`, advertising `modes` (all at
    /// the same resolution) in addition to `current`.
    fn output_running_at(name: &str, current: f64, modes: &[f64]) -> Output {
        let mode = |refresh_hz| crate::model::Mode {
            width: 1920,
            height: 1080,
            refresh_hz,
        };
        Output {
            name: name.to_string(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: Some(mode(current)),
            modes: std::iter::once(current)
                .chain(modes.iter().copied())
                .map(mode)
                .collect(),
            rect: crate::model::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    #[test]
    fn no_displays_is_a_pass() {
        let (status, detail) = refresh_rate_verdict(&[], &[]);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(detail, "no displays attached");
    }

    #[test]
    fn a_single_display_is_a_pass() {
        let outputs = vec![output_running_at("DP-1", 60.0, &[])];
        let (status, detail) = refresh_rate_verdict(&outputs, &[]);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(
            detail,
            "one display attached (DP-1 at 60 Hz); nothing to keep in step"
        );
    }

    #[test]
    fn synthetic_outputs_do_not_count_towards_the_pair() {
        let outputs = vec![
            output_running_at("DP-1", 60.0, &[]),
            output_running_at("HEADLESS-1", 59.0, &[]),
        ];
        let (status, detail) = refresh_rate_verdict(&outputs, &[]);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("one display attached"), "{detail}");
    }

    #[test]
    fn matched_rates_pass() {
        let outputs = vec![
            output_running_at("DP-1", 60.0, &[]),
            output_running_at("DP-3", 60.0, &[]),
        ];
        let (status, detail) = refresh_rate_verdict(&outputs, &[]);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(detail, "DP-1, DP-3 all run at 60 Hz");
    }

    #[test]
    fn mismatched_rates_warn_and_name_both_outputs() {
        // Brain's two projectors, verbatim: both advertise both rates.
        let outputs = vec![
            output_running_at("DP-1", 60.0, &[59.939]),
            output_running_at("DP-3", 59.939, &[60.0]),
        ];
        let (status, detail) = refresh_rate_verdict(&outputs, &[]);
        assert_eq!(status, CheckStatus::Warn);
        assert!(
            detail.starts_with("DP-1 runs at 60 Hz but DP-3 runs at 59.939 Hz."),
            "{detail}"
        );
        assert!(detail.contains("16 s"), "{detail}");
        assert!(
            detail.contains("All of them advertise 60 Hz at their current resolution"),
            "{detail}"
        );
        assert!(detail.contains("set that on DP-3"), "{detail}");
    }

    #[test]
    fn no_common_rate_says_so() {
        let outputs = vec![
            output_running_at("DP-1", 60.0, &[]),
            output_running_at("DP-3", 50.0, &[]),
        ];
        let (status, detail) = refresh_rate_verdict(&outputs, &[]);
        assert_eq!(status, CheckStatus::Warn);
        assert!(
            detail.contains("They advertise no common rate at their current resolutions"),
            "{detail}"
        );
    }

    #[test]
    fn a_configuration_that_asks_for_different_rates_is_called_out() {
        let outputs = vec![
            output_running_at("DP-1", 60.0, &[59.939]),
            output_running_at("DP-3", 59.939, &[60.0]),
        ];
        let configured = vec![
            {
                let mut entry =
                    crate::model::OutputConfig::new(crate::model::OutputMatch::by_name("DP-1"));
                entry.mode = Some(crate::model::Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60.0,
                });
                entry
            },
            {
                let mut entry =
                    crate::model::OutputConfig::new(crate::model::OutputMatch::by_name("DP-3"));
                entry.mode = Some(crate::model::Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 59.939,
                });
                entry
            },
        ];
        let (status, detail) = refresh_rate_verdict(&outputs, &configured);
        assert_eq!(status, CheckStatus::Warn);
        assert!(
            detail.ends_with("The configuration asks for these rates."),
            "{detail}"
        );
    }

    // --- output_phase_verdict --------------------------------------------

    fn timing(name: &str, phase_ms: Option<f64>) -> OutputTiming {
        OutputTiming {
            name: name.to_string(),
            presented: 330,
            discarded: 0,
            zero_copy_presented: 0,
            refresh_hz: Some(60.0),
            phase_ms,
            phase_spread_ms: Some(0.1),
            lag_frames: LagFrames::default(),
        }
    }

    fn stats_with(presentation_feedback: bool, outputs: Vec<OutputTiming>) -> ProjectionStats {
        ProjectionStats {
            measured_at: 1,
            interval_seconds: 10.0,
            free_run: false,
            canvas_fps: 60.0,
            presented_fps: 60.0,
            frames_superseded: 0,
            stalls: 0,
            per_frame_ms: crate::model::FrameCost {
                waiting: 1.0,
                snapshot: 1.0,
                requesting: 1.0,
                blending: 1.0,
                gpu: 0.0,
            },
            presentation_feedback,
            offset_ms: None,
            straddles: 0,
            gate_holds: 0,
            renderer: "cpu".to_string(),
            capture_intervals: crate::model::CaptureIntervals::default(),
            outputs,
        }
    }

    #[test]
    fn no_slicer_running_is_a_pass() {
        let (status, detail) = output_phase_verdict(false, None);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(detail, "not measured: no slicer is running");
    }

    #[test]
    fn a_running_but_silent_slicer_is_a_pass() {
        // The 2026-09-15 bench, verbatim: alive and logged, but the active
        // page was static so the damage-driven frame loop had nothing to
        // capture. Must read as normal, not as the slicer being down.
        let (status, detail) = output_phase_verdict(true, None);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(
            detail,
            "not measured: the slicer has reported no frames yet, which is what a \
             static page looks like — the frame loop only runs when the content draws"
        );
    }

    #[test]
    fn no_presentation_feedback_is_a_pass() {
        let stats = stats_with(false, vec![timing("DP-5", None), timing("DP-6", None)]);
        let (status, detail) = output_phase_verdict(true, Some(&stats));
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(
            detail,
            "the compositor offers no presentation timing, so phase cannot be measured"
        );
    }

    #[test]
    fn fewer_than_two_measured_outputs_is_a_pass() {
        let stats = stats_with(true, vec![timing("DP-5", Some(0.0))]);
        let (status, _detail) = output_phase_verdict(true, Some(&stats));
        assert_eq!(status, CheckStatus::Pass);
    }

    #[test]
    fn tightly_locked_outputs_pass() {
        // The rig's batched-enable result, verbatim.
        let stats = stats_with(
            true,
            vec![
                timing("DP-5", Some(0.0)),
                timing("DP-6", Some(0.03)),
                timing("DP-7", Some(-0.02)),
                timing("DP-8", Some(0.01)),
            ],
        );
        let (status, detail) = output_phase_verdict(true, Some(&stats));
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(detail, "DP-5, DP-6, DP-7, DP-8 within 0.03 ms");
    }

    #[test]
    fn the_reference_out_of_phase_with_a_locked_group_is_named_as_the_odd_one() {
        // The rig's one-at-a-time result, verbatim: the first head (the
        // measurement's own reference, so its own phase reads 0.0) ends up
        // 7.1 ms from the other three, which lock to each other.
        let stats = stats_with(
            true,
            vec![
                timing("DP-5", Some(0.0)),
                timing("DP-6", Some(7.1)),
                timing("DP-7", Some(7.1)),
                timing("DP-8", Some(7.1)),
            ],
        );
        let (status, detail) = output_phase_verdict(true, Some(&stats));
        assert_eq!(status, CheckStatus::Warn);
        assert_eq!(
            detail,
            "DP-5 is 7.1 ms out of phase with DP-6, DP-7, DP-8 (measured from \
             presentation timestamps). Heads that were enabled one at a time start \
             their rasters at different moments; the same frame then lands on \
             different refreshes for a quarter of the time. Re-enabling every \
             output together puts them in phase."
        );
    }

    #[test]
    fn a_lone_output_out_of_phase_with_the_reference_is_named_individually() {
        let stats = stats_with(
            true,
            vec![
                timing("DP-5", Some(0.0)),
                timing("DP-6", Some(0.02)),
                timing("DP-7", Some(-3.4)),
            ],
        );
        let (status, detail) = output_phase_verdict(true, Some(&stats));
        assert_eq!(status, CheckStatus::Warn);
        assert!(
            detail.starts_with("out of phase with DP-5: DP-7 -3.4 ms"),
            "{detail}"
        );
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
        let (trigger, _receiver) = crate::reconciler::Reconciler::channel();
        let runner = CheckRunner::new(
            bootstrap,
            sway,
            Arc::new(MockAudio::default()),
            Arc::new(crate::state::StateStore::ephemeral(
                dir.path().to_path_buf(),
            )),
            EventHub::new(),
            Arc::new(crate::capabilities::CapabilityStore::new(dir.path())),
            Arc::new(Snapshot::new()),
            trigger,
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
        // rather than silently writing a drop-in nothing will read. That is the
        // ordinary case on an appliance provisioned by provision.sh: sway comes
        // from the login profile on tty1, and only that profile can follow
        // suede.toml — so the message has to send the operator at the session,
        // not at a unit that does not exist.
        let dir = tempfile::tempdir().unwrap();
        for direct_scanout in [true, false] {
            let result = runner_slicing(dir.path().to_path_buf(), direct_scanout)
                .fix(ids::DIRECT_SCANOUT)
                .await;
            if let Err(error) = result {
                assert!(matches!(error, ApiError::Validation(_)));
                let text = error.to_string();
                assert!(text.contains("WLR_SCENE_DISABLE_DIRECT_SCANOUT"), "{text}");
                assert!(text.contains("restart the session"), "{text}");
                assert!(text.contains("suede.toml"), "{text}");
            }
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
                    ids::DIRECT_SCANOUT,
                    ids::OUTPUT_PHASE,
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
