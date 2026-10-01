//! Experimental direct presentation: the slicer drives the physical outputs
//! itself through `VK_KHR_display`, while the compositor is a headless-only
//! Sway that only ever holds the canvas.
//!
//! This module holds:
//!
//! - what the daemon hands the slicer — the [`DirectDisplayConfig`] — and
//!   the pure derivation of it from a slicer spec, the observed (simulated)
//!   outputs, and the live [`crate::drm_inventory::DrmInventory`];
//! - the session lifecycle: [`resolve`], which decides at startup whether
//!   this session presents directly (the file's own answer is
//!   [`crate::config::BootstrapConfig::presentation_effective`]; the login
//!   profile in `packaging/provision.sh` makes the same decision before the
//!   compositor starts), and [`DirectSession`], which confirms a direct
//!   session, keeps its crash budget, and falls back to the ordinary Wayland
//!   session for the rest of the boot when direct presentation fails.
//!
//! How the physical outputs are simulated towards the rest of the daemon is
//! [`crate::sway::direct`].
//!
//! ## Runtime state
//!
//! Shared with the login profile, in `$XDG_RUNTIME_DIR/suede` (a tmpfs, so
//! all of it is gone after a reboot and direct is tried again):
//!
//! - `session`: what the last login started, `direct` or `wayland`;
//! - `direct-attempts`: headless starts this boot that the daemon has not
//!   yet confirmed; the profile falls back on the fourth login;
//! - `presentation-fallback`: a [`FallbackMarker`]. While it exists the
//!   profile starts the ordinary session and the daemon reports its reason.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::{Output, PresentationMode, ProjectionStats};

/// Which card and which connectors, at which modes, the slicer presents to.
/// Outputs are in slice order: `outputs[i]` presents `spec.slices[i]`.
///
/// Serialized verbatim to `suede slice --presentation-config-json` (or read
/// from `--presentation-config <path>` by the test harness).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectDisplayConfig {
    pub card: PathBuf,
    pub outputs: Vec<DirectOutputConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectOutputConfig {
    pub name: String,
    pub connector_id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

/// Build the slicer's direct configuration for `spec`.
///
/// One entry per slice, in slice order (the slicer pairs them by index and
/// checks the names). Each takes its connector from the live inventory and
/// its mode from the output's current (simulated) mode, with the refresh
/// rate recovered to the inventory's exact millihertz rather than
/// re-derived from Sway's rounded hertz.
///
/// Fails, rather than guessing, when the inventory does not pass its
/// preflight (one card, every connected output addressable and
/// identified), when a slice names an output the inventory does not know or
/// that is not enabled, or when the spec pins the CPU renderer, which cannot
/// present directly.
#[cfg(feature = "projection")]
pub fn direct_config_for(
    spec: &crate::projection::SlicerSpec,
    outputs: &[crate::model::Output],
    inventory: &crate::drm_inventory::DrmInventory,
) -> Result<DirectDisplayConfig, String> {
    if spec.renderer == crate::model::Renderer::Cpu {
        return Err(
            "direct presentation needs the GPU renderer, but projection.renderer is cpu"
                .to_string(),
        );
    }
    let card = inventory.preflight()?;
    let mut configured = Vec::with_capacity(spec.slices.len());
    for slice in &spec.slices {
        let name = &slice.output;
        let physical = inventory
            .get(name)
            .filter(|output| output.connected)
            .ok_or_else(|| format!("{name} is not a connected display on {}", card.display()))?;
        let connector_id = physical
            .connector_id
            .ok_or_else(|| format!("{name} has no connector_id"))?;
        let observed = outputs
            .iter()
            .find(|output| &output.name == name)
            .ok_or_else(|| format!("{name} is not among the observed outputs"))?;
        let mode = observed
            .current_mode
            .filter(|_| observed.active)
            .ok_or_else(|| format!("{name} is not enabled, so it has no mode to present at"))?;
        let refresh_millihz = physical
            .exact_refresh_millihz(mode.width, mode.height, mode.refresh_hz)
            .ok_or_else(|| {
                format!(
                    "{name} does not advertise {} in the DRM inventory",
                    mode.to_sway()
                )
            })?;
        configured.push(DirectOutputConfig {
            name: name.clone(),
            connector_id,
            width: u32::try_from(mode.width).map_err(|_| format!("{name} has a negative width"))?,
            height: u32::try_from(mode.height)
                .map_err(|_| format!("{name} has a negative height"))?,
            refresh_millihz: u32::try_from(refresh_millihz)
                .map_err(|_| format!("{name} has a negative refresh rate"))?,
        });
    }
    if configured.is_empty() {
        return Err("the slicer spec has no slices to present".to_string());
    }
    Ok(DirectDisplayConfig {
        card,
        outputs: configured,
    })
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// Headless starts per boot the login profile makes without the daemon
/// confirming one, before it writes the fallback marker itself.
/// `packaging/provision.sh` (the PRESENTATION_EOF block) has its own copy.
pub const DIRECT_START_ATTEMPTS: u32 = 3;
/// Unexpected slicer exits, after confirmation, that end a direct session…
pub const CRASH_BUDGET_EXITS: usize = 3;
/// …when they fall within this long of each other.
pub const CRASH_BUDGET_WINDOW: Duration = Duration::from_secs(10 * 60);
/// How long `suede display-reset` may take before it is abandoned.
const DISPLAY_RESET_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the headless compositor gets to exit once asked.
const COMPOSITOR_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a login started, as the profile recorded it in `session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Wayland,
    Direct,
}

/// Why direct presentation was abandoned this boot. Written by the daemon
/// when it falls back, or by the login profile when the headless compositor
/// failed to start [`DIRECT_START_ATTEMPTS`] times.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackMarker {
    pub reason: String,
    /// Unix seconds.
    #[serde(default)]
    pub time: u64,
    /// `/proc/sys/kernel/random/boot_id` when it was written. The marker
    /// lives on a tmpfs, so this is belt and braces: a marker from another
    /// boot is ignored.
    #[serde(default)]
    pub boot_id: Option<String>,
}

/// The runtime files shared with the login profile. See the module docs.
#[derive(Clone, Debug)]
pub struct RuntimeState {
    dir: PathBuf,
}

impl RuntimeState {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `$XDG_RUNTIME_DIR/suede`, where the login profile writes.
    pub fn from_env() -> Option<Self> {
        crate::util::runtime_dir().map(|dir| Self::new(dir.join("suede")))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// What the last login started, if it recorded it.
    pub fn session(&self) -> Option<SessionKind> {
        match std::fs::read_to_string(self.path("session")).ok()?.trim() {
            "direct" => Some(SessionKind::Direct),
            "wayland" => Some(SessionKind::Wayland),
            _ => None,
        }
    }

    /// The fallback marker for this boot, if there is one.
    pub fn fallback(&self) -> Option<FallbackMarker> {
        self.fallback_for_boot(current_boot_id().as_deref())
    }

    /// [`Self::fallback`] against an explicit boot id. A marker that cannot
    /// be parsed still counts — the profile honors its mere existence, and
    /// the daemon must agree with it — with a reason saying so.
    pub fn fallback_for_boot(&self, boot_id: Option<&str>) -> Option<FallbackMarker> {
        let text = std::fs::read_to_string(self.path("presentation-fallback")).ok()?;
        match serde_json::from_str::<FallbackMarker>(&text) {
            Ok(marker) => {
                let other_boot = matches!(
                    (marker.boot_id.as_deref(), boot_id),
                    (Some(written), Some(now)) if !written.is_empty() && written != now
                );
                (!other_boot).then_some(marker)
            }
            Err(error) => Some(FallbackMarker {
                reason: format!("fallback marker present but unreadable ({error})"),
                time: 0,
                boot_id: None,
            }),
        }
    }

    /// Record a fallback. The first reason this boot wins: an existing
    /// marker is left alone, so a later, secondary failure cannot hide the
    /// one that started it.
    pub fn write_fallback(&self, reason: &str) -> std::io::Result<()> {
        if self.fallback().is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let marker = FallbackMarker {
            reason: reason.to_string(),
            time: crate::util::unix_now(),
            boot_id: current_boot_id(),
        };
        let json = serde_json::to_string(&marker).map_err(std::io::Error::other)?;
        let staging = self.path(".presentation-fallback.tmp");
        std::fs::write(&staging, format!("{json}\n"))?;
        std::fs::rename(&staging, self.path("presentation-fallback"))
    }

    /// Forget the unconfirmed headless starts: direct presentation is
    /// working, so later compositor restarts this boot start from zero.
    pub fn clear_direct_attempts(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.path("direct-attempts")) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

fn current_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Whether a compositor has no outputs but headless ones — the shape of
/// the session the login profile starts for direct presentation.
pub fn headless_only(outputs: &[Output]) -> bool {
    !outputs.is_empty()
        && outputs
            .iter()
            .all(|output| output.name.starts_with("HEADLESS-"))
}

/// Everything [`resolve`] decides from.
#[derive(Clone, Debug)]
pub struct ResolutionInput {
    /// The bootstrap file's own answer, with its reason:
    /// [`crate::config::BootstrapConfig::presentation_effective`].
    pub file: (PresentationMode, Option<String>),
    /// This boot's fallback marker's reason, if there is one.
    pub fallback: Option<String>,
    /// Whether the compositor the daemon connected to has only headless
    /// outputs. `false` when it was not asked (see [`needs_compositor`]).
    pub headless_only: bool,
    /// What the login profile says it started.
    pub session: Option<SessionKind>,
}

/// What this session does about presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Present directly: the compositor was started for it.
    Direct,
    /// Present through the compositor, as every appliance always has.
    /// `reason` says why when direct was asked for.
    Wayland { reason: Option<String> },
    /// A headless-only compositor started for direct presentation is still
    /// running, but this session must not present directly. End it, so the
    /// login profile starts the ordinary session instead.
    FinishSwitch { reason: String },
}

/// The reason reported while `presentation = "direct"` finds a compositor
/// that drives the displays itself.
pub const SESSION_NOT_STARTED_REASON: &str =
    "presentation = \"direct\" is set, but this session was not started for direct \
     presentation: the compositor drives the displays itself. Restart the session \
     (sudo systemctl restart getty@tty1) to start one.";

/// Whether [`resolve`] needs to know what the compositor looks like. Only
/// when direct is wanted or the last login started a direct session — so
/// an appliance that never asked for direct presentation starts exactly as
/// it always has, without an extra query.
pub fn needs_compositor(file: PresentationMode, session: Option<SessionKind>) -> bool {
    file == PresentationMode::Direct || session == Some(SessionKind::Direct)
}

/// Decide this session's presentation. Pure; the table:
///
/// | File wants | Marker | Compositor | Result |
/// | --- | --- | --- | --- |
/// | direct | none | headless-only | direct |
/// | direct | none | has real outputs | wayland, "session not started for direct" |
/// | direct | present | headless-only, `session = direct` | finish the switch |
/// | direct | present | anything else | wayland, the marker's reason |
/// | wayland | any | headless-only, `session = direct` | finish the switch |
/// | wayland | any | anything else | wayland (the file's reason, if any) |
///
/// "File wants" is already the file's own resolution, so `direct` without
/// `allow_overlaps` arrives here as wayland with its reason.
pub fn resolve(input: &ResolutionInput) -> Resolution {
    let left_over_direct = input.headless_only && input.session == Some(SessionKind::Direct);
    let (file, file_reason) = &input.file;
    match (file, input.fallback.as_deref()) {
        (PresentationMode::Direct, None) => {
            if input.headless_only {
                Resolution::Direct
            } else {
                Resolution::Wayland {
                    reason: Some(SESSION_NOT_STARTED_REASON.to_string()),
                }
            }
        }
        (PresentationMode::Direct, Some(marker)) => {
            let reason = format!(
                "direct presentation fell back to wayland for the rest of this boot \
                 (it is tried again after a reboot): {marker}"
            );
            if left_over_direct {
                Resolution::FinishSwitch { reason }
            } else {
                Resolution::Wayland {
                    reason: Some(reason),
                }
            }
        }
        (PresentationMode::Wayland, _) => {
            if left_over_direct {
                Resolution::FinishSwitch {
                    reason: file_reason.clone().unwrap_or_else(|| {
                        "presentation is wayland, but the compositor was started for \
                         direct presentation"
                            .to_string()
                    }),
                }
            } else {
                Resolution::Wayland {
                    reason: file_reason.clone(),
                }
            }
        }
    }
}

/// Unexpected exits within a sliding window.
#[derive(Clone, Debug)]
pub struct CrashBudget {
    exits: VecDeque<Instant>,
    limit: usize,
    window: Duration,
}

impl CrashBudget {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            exits: VecDeque::new(),
            limit,
            window,
        }
    }

    /// Record an exit at `now`; whether the budget is now spent.
    pub fn record(&mut self, now: Instant) -> bool {
        while self
            .exits
            .front()
            .is_some_and(|&first| now.saturating_duration_since(first) >= self.window)
        {
            self.exits.pop_front();
        }
        self.exits.push_back(now);
        self.exits.len() >= self.limit
    }
}

/// Whether a stats interval shows direct presentation working: frames
/// presented through `VK_KHR_display`.
pub fn confirms(stats: &ProjectionStats) -> bool {
    stats.presentation_backend.as_deref() == Some("vulkan-display")
        && (stats.presented_fps > 0.0 || stats.outputs.iter().any(|output| output.presented > 0))
}

/// Confirmation and the crash budget of one direct session. Pure.
#[derive(Clone, Debug)]
pub struct DirectHealth {
    confirmed: bool,
    budget: CrashBudget,
}

impl Default for DirectHealth {
    fn default() -> Self {
        Self {
            confirmed: false,
            budget: CrashBudget::new(CRASH_BUDGET_EXITS, CRASH_BUDGET_WINDOW),
        }
    }
}

impl DirectHealth {
    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// Take a stats interval into account; true only for the one that
    /// confirms the session.
    pub fn observe(&mut self, stats: &ProjectionStats) -> bool {
        if self.confirmed || !confirms(stats) {
            return false;
        }
        self.confirmed = true;
        true
    }

    /// A slicer exit the daemon did not cause. The reason to fall back, if
    /// this one means falling back: any exit before confirmation (it never
    /// worked — DRM master denied, a mode or extension missing), and after
    /// it, the one that spends the crash budget. `stderr_line` is the
    /// slicer's last non-empty stderr line, if any was captured — it usually
    /// names the actual cause (see [`crate::projection::gpu::display`]'s
    /// capability refusal message), where `detail` alone only ever has the
    /// exit status.
    pub fn slicer_exited(
        &mut self,
        now: Instant,
        detail: &str,
        stderr_line: Option<&str>,
    ) -> Option<String> {
        let reason = if !self.confirmed {
            Some(format!(
                "the slicer exited before direct presentation was confirmed ({detail})"
            ))
        } else {
            self.budget.record(now).then(|| {
                format!(
                    "the slicer exited {CRASH_BUDGET_EXITS} times within {} minutes (last: {detail})",
                    CRASH_BUDGET_WINDOW.as_secs() / 60
                )
            })
        };
        reason.map(|reason| append_stderr_line(reason, stderr_line))
    }
}

/// A fallback reason's last stderr line is capped this long, so a runaway or
/// binary line read from the slicer's stderr cannot blow up the marker file
/// or `/system.presentation.reason`. Also used by
/// [`crate::projection::manager`]'s stderr reader to cap what it keeps
/// between exits, so the two stay in step.
pub(crate) const MAX_STDERR_REASON_CHARS: usize = 300;

/// Append the slicer's last stderr line to a fallback `reason`, trimmed and
/// capped at [`MAX_STDERR_REASON_CHARS`] characters; `reason` unchanged when
/// there is no line, or it is empty once trimmed.
fn append_stderr_line(reason: String, stderr_line: Option<&str>) -> String {
    match stderr_line.map(str::trim).filter(|line| !line.is_empty()) {
        Some(line) => {
            let truncated: String = line.chars().take(MAX_STDERR_REASON_CHARS).collect();
            format!("{reason}: {truncated}")
        }
        None => reason,
    }
}

/// Why a direct session is ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionExit {
    /// Direct presentation failed; the marker is written, and the daemon
    /// ends the headless compositor so the login starts the ordinary one.
    Fallback(String),
    /// The headless compositor went away on its own (a crash, or the
    /// operator restarting the session). The daemon only exits: systemd
    /// restarts it and it resolves again against whatever comes up next.
    CompositorLost(String),
}

/// One direct session's lifecycle: confirmation, crash budget, and the one
/// request to end it. Shared by the reconciler (slicer exits, derivation
/// failures), the stats and compositor watchers, and `main` (which acts on
/// the exit).
pub struct DirectSession {
    runtime: Option<RuntimeState>,
    health: Mutex<DirectHealth>,
    exit: tokio::sync::watch::Sender<Option<SessionExit>>,
}

impl DirectSession {
    pub fn new(runtime: Option<RuntimeState>) -> Arc<Self> {
        let (exit, _) = tokio::sync::watch::channel(None);
        Arc::new(Self {
            runtime,
            health: Mutex::new(DirectHealth::default()),
            exit,
        })
    }

    /// Whether this session is ending. Once it is, nothing may start a
    /// slicer: a stale direct slicer must never meet the next compositor.
    pub fn exit_requested(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// Why this session is ending, if it is.
    pub fn exit_reason(&self) -> Option<SessionExit> {
        self.exit.borrow().clone()
    }

    pub fn confirmed(&self) -> bool {
        self.health.lock().unwrap().confirmed()
    }

    /// The first request wins.
    fn request_exit(&self, exit: SessionExit) -> bool {
        self.exit.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(exit);
            true
        })
    }

    /// Give up on direct presentation for the rest of this boot. The marker
    /// is written first, so however the rest of the routine is interrupted,
    /// the next login starts the ordinary session.
    pub fn fall_back(&self, reason: impl Into<String>) {
        let reason = reason.into();
        if self.exit_requested() {
            return;
        }
        tracing::error!(%reason, "direct presentation failed; falling back to wayland for the rest of this boot");
        if let Some(runtime) = &self.runtime {
            if let Err(error) = runtime.write_fallback(&reason) {
                tracing::error!(%error, "could not write the presentation fallback marker");
            }
        }
        self.request_exit(SessionExit::Fallback(reason));
    }

    /// The headless compositor has gone away.
    pub fn compositor_lost(&self, reason: impl Into<String>) {
        let reason = reason.into();
        if self.request_exit(SessionExit::CompositorLost(reason.clone())) {
            tracing::warn!(%reason, "the direct session's compositor went away; exiting so the next session is resolved afresh");
        }
    }

    /// A slicer exit the daemon did not cause. `stderr_line` is its last
    /// non-empty stderr line, if the manager captured one.
    pub fn slicer_exited(&self, detail: &str, stderr_line: Option<&str>) {
        if self.exit_requested() {
            return;
        }
        let verdict =
            self.health
                .lock()
                .unwrap()
                .slicer_exited(Instant::now(), detail, stderr_line);
        match verdict {
            Some(reason) => self.fall_back(reason),
            None => {
                tracing::warn!(
                    detail,
                    ?stderr_line,
                    "the direct slicer exited; respawning it"
                )
            }
        }
    }

    /// Take a stats interval into account; whether the session is
    /// confirmed afterwards.
    pub fn observe_stats(&self, stats: &ProjectionStats) -> bool {
        let mut health = self.health.lock().unwrap();
        if health.observe(stats) {
            tracing::info!(
                presented_fps = stats.presented_fps,
                "direct presentation confirmed"
            );
            if let Some(runtime) = &self.runtime {
                if let Err(error) = runtime.clear_direct_attempts() {
                    tracing::warn!(%error, "could not clear the direct start attempts");
                }
            }
        }
        health.confirmed()
    }

    /// Resolves once this session is ending.
    pub async fn wait_for_exit(&self) -> SessionExit {
        let mut receiver = self.exit.subscribe();
        loop {
            if let Some(exit) = receiver.borrow_and_update().clone() {
                return exit;
            }
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Confirm the session from the slicer's stats, then stop watching.
    pub async fn watch_stats(
        self: Arc<Self>,
        events: crate::events::EventHub,
        snapshot: Arc<crate::snapshot::Snapshot>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        use tokio::sync::broadcast::error::RecvError;
        let mut receiver = events.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                event = receiver.recv() => match event {
                    Ok(crate::events::ServerEvent::ProjectionStatsChanged(_))
                    | Err(RecvError::Lagged(_)) => {
                        if let Some(stats) = snapshot.projection_stats() {
                            if self.observe_stats(&stats) {
                                return;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                },
            }
        }
    }

    /// Watch the headless compositor, and end the session when it goes:
    /// its `shutdown` event, or its socket disappearing (a killed sway
    /// sends no event, but its socket's owner is gone).
    pub async fn watch_compositor(
        self: Arc<Self>,
        sway: Arc<dyn crate::sway::SwayClient>,
        socket: PathBuf,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        use tokio::sync::broadcast::error::RecvError;
        let mut events = sway.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                event = events.recv() => match event {
                    Ok(crate::sway::SwayEvent::Shutdown) => {
                        self.compositor_lost("the headless compositor is shutting down");
                        return;
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    if !compositor_alive(&socket) {
                        self.compositor_lost(format!(
                            "the headless compositor's socket {} is gone",
                            socket.display()
                        ));
                        return;
                    }
                }
            }
        }
    }
}

fn compositor_alive(socket: &Path) -> bool {
    socket.exists() && crate::sway::socket_owner_alive(socket)
}

/// Run `suede display-reset` as a child, bounded: clear an NVKMS grant a
/// killed slicer left behind. Harmless when there is none, and when another
/// process holds DRM master. Never fails; what it said is logged.
pub async fn run_display_reset() {
    let program = std::env::current_exe()
        .ok()
        // Replaced on disk since this process started: use the installed one.
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/suede"));
    let mut command = tokio::process::Command::new(&program);
    command
        .arg("display-reset")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(DISPLAY_RESET_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => {
            let said = String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr);
            tracing::info!(status = %output.status, output = said.trim(), "display reset");
        }
        Ok(Err(error)) => tracing::warn!(%error, "could not run display-reset"),
        Err(_) => tracing::warn!(
            timeout_s = DISPLAY_RESET_TIMEOUT.as_secs(),
            "display-reset did not finish in time"
        ),
    }
}

/// End a headless compositor started for direct presentation, so getty's
/// auto-login starts the session the login profile now chooses. The slicer
/// must already be stopped. Clears any stale NVKMS grant, asks sway to exit,
/// and waits (bounded) for its socket to go.
pub async fn end_headless_compositor(sway: &dyn crate::sway::SwayClient, socket: &Path) {
    run_display_reset().await;
    tracing::warn!("ending the headless compositor so the login starts the ordinary session");
    // Sway may exit before it answers; either way is fine.
    let _ = tokio::time::timeout(Duration::from_secs(2), sway.run_command("exit")).await;
    let deadline = Instant::now() + COMPOSITOR_EXIT_TIMEOUT;
    while compositor_alive(socket) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if compositor_alive(socket) {
        tracing::warn!(
            timeout_s = COMPOSITOR_EXIT_TIMEOUT.as_secs(),
            "the headless compositor did not exit in time"
        );
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::model::PresentationMode::{Direct, Wayland};

    fn input(
        file: PresentationMode,
        fallback: Option<&str>,
        headless_only: bool,
        session: Option<SessionKind>,
    ) -> ResolutionInput {
        ResolutionInput {
            file: (file, None),
            fallback: fallback.map(str::to_string),
            headless_only,
            session,
        }
    }

    #[test]
    fn direct_with_a_headless_compositor_presents_directly() {
        for session in [Some(SessionKind::Direct), None] {
            assert_eq!(
                resolve(&input(Direct, None, true, session)),
                Resolution::Direct
            );
        }
    }

    #[test]
    fn direct_with_a_drm_compositor_is_wayland_with_a_reason_and_no_action() {
        for session in [Some(SessionKind::Wayland), Some(SessionKind::Direct), None] {
            assert_eq!(
                resolve(&input(Direct, None, false, session)),
                Resolution::Wayland {
                    reason: Some(SESSION_NOT_STARTED_REASON.to_string())
                }
            );
        }
    }

    #[test]
    fn a_fallback_marker_finishes_a_left_over_direct_session() {
        match resolve(&input(
            Direct,
            Some("slicer died"),
            true,
            Some(SessionKind::Direct),
        )) {
            Resolution::FinishSwitch { reason } => {
                assert!(reason.contains("slicer died"), "{reason}");
                assert!(reason.contains("rest of this boot"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_fallback_marker_reports_its_reason_on_the_ordinary_session() {
        match resolve(&input(
            Direct,
            Some("slicer died"),
            false,
            Some(SessionKind::Wayland),
        )) {
            Resolution::Wayland {
                reason: Some(reason),
            } => assert!(reason.contains("slicer died"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wayland_finishes_a_left_over_direct_session() {
        assert!(matches!(
            resolve(&input(Wayland, None, true, Some(SessionKind::Direct))),
            Resolution::FinishSwitch { .. }
        ));
        // The file's reason (direct without allow_overlaps) is carried.
        let file = ResolutionInput {
            file: (Wayland, Some("needs allow_overlaps".to_string())),
            fallback: None,
            headless_only: true,
            session: Some(SessionKind::Direct),
        };
        assert_eq!(
            resolve(&file),
            Resolution::FinishSwitch {
                reason: "needs allow_overlaps".to_string()
            }
        );
    }

    #[test]
    fn wayland_leaves_every_other_compositor_alone() {
        // A headless-only compositor nobody started for direct (a dev or CI
        // sway) is not ended: only a recorded direct session is.
        for (headless_only, session) in [
            (true, None),
            (true, Some(SessionKind::Wayland)),
            (false, None),
            (false, Some(SessionKind::Direct)),
            (false, Some(SessionKind::Wayland)),
        ] {
            assert_eq!(
                resolve(&input(Wayland, None, headless_only, session)),
                Resolution::Wayland { reason: None },
                "{headless_only} {session:?}"
            );
            assert_eq!(
                resolve(&input(Wayland, Some("old"), headless_only, session)),
                Resolution::Wayland { reason: None },
            );
        }
    }

    #[test]
    fn the_compositor_is_only_consulted_when_direct_is_in_play() {
        assert!(!needs_compositor(Wayland, None));
        assert!(!needs_compositor(Wayland, Some(SessionKind::Wayland)));
        assert!(needs_compositor(Wayland, Some(SessionKind::Direct)));
        assert!(needs_compositor(Direct, None));
    }

    #[test]
    fn headless_only_means_nothing_but_headless_outputs() {
        let output = |name: &str| Output {
            name: name.to_string(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: crate::model::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        };
        assert!(headless_only(&[output("HEADLESS-1")]));
        assert!(!headless_only(&[output("HEADLESS-1"), output("DP-1")]));
        assert!(!headless_only(&[]));
    }

    #[test]
    fn the_crash_budget_is_three_exits_in_ten_minutes() {
        let start = Instant::now();
        let minutes = |m: u64| start + Duration::from_secs(m * 60);
        let mut budget = CrashBudget::new(CRASH_BUDGET_EXITS, CRASH_BUDGET_WINDOW);
        assert!(!budget.record(minutes(0)));
        assert!(!budget.record(minutes(4)));
        // The first has aged out by minute 10: two in the window, not three.
        assert!(!budget.record(minutes(10)));
        assert!(
            budget.record(minutes(13)),
            "4, 10 and 13 are within ten minutes"
        );

        let mut quick = CrashBudget::new(CRASH_BUDGET_EXITS, CRASH_BUDGET_WINDOW);
        assert!(!quick.record(start));
        assert!(!quick.record(start + Duration::from_secs(5)));
        assert!(quick.record(start + Duration::from_secs(10)));
    }

    fn stats(backend: Option<&str>, presented_fps: f64) -> ProjectionStats {
        serde_json::from_value(serde_json::json!({
            "measuredAt": 10,
            "intervalSeconds": 10.0,
            "freeRun": false,
            "canvasFps": 60.0,
            "presentedFps": presented_fps,
            "framesSuperseded": 0,
            "stalls": 0,
            "perFrameMs": {"waiting": 0.1, "snapshot": 0.1, "requesting": 0.1, "blending": 0.1, "gpu": 0.0},
            "presentationFeedback": true,
            "offsetMs": null,
            "straddles": 0,
            "gateHolds": 0,
            "renderer": "gpu",
            "presentationBackend": backend,
            "captureIntervals": {"one": 0, "two": 0, "three": 0, "more": 0},
            "outputs": []
        }))
        .unwrap()
    }

    #[test]
    fn only_frames_presented_through_vulkan_display_confirm() {
        assert!(confirms(&stats(Some("vulkan-display"), 59.9)));
        assert!(!confirms(&stats(Some("vulkan-display"), 0.0)));
        assert!(!confirms(&stats(Some("wayland"), 59.9)));
        assert!(!confirms(&stats(None, 59.9)));
    }

    #[test]
    fn any_exit_before_confirmation_falls_back() {
        let mut health = DirectHealth::default();
        let reason = health
            .slicer_exited(Instant::now(), "exit status: 1", None)
            .expect("never confirmed: fall back");
        assert!(reason.contains("before direct presentation was confirmed"));
        assert!(reason.contains("exit status: 1"));
    }

    #[test]
    fn the_last_stderr_line_is_named_in_the_reason() {
        let mut health = DirectHealth::default();
        let reason = health
            .slicer_exited(
                Instant::now(),
                "exit status: 1",
                Some(
                    "direct presentation needs VK_EXT_present_timing and VK_KHR_present_id2; \
                      this device/driver (NVIDIA RTX A1000, 550.163.01) exposes neither",
                ),
            )
            .expect("never confirmed: fall back");
        assert!(reason.contains("exit status: 1"), "{reason}");
        assert!(reason.contains("exposes neither"), "{reason}");
        // No line, or a blank one, changes nothing.
        let mut health = DirectHealth::default();
        let plain = health
            .slicer_exited(Instant::now(), "exit status: 1", None)
            .unwrap();
        let mut health = DirectHealth::default();
        let blank = health
            .slicer_exited(Instant::now(), "exit status: 1", Some("   "))
            .unwrap();
        assert_eq!(plain, blank);
    }

    #[test]
    fn a_long_stderr_line_is_trimmed_and_capped_at_300_characters() {
        let mut health = DirectHealth::default();
        let line = format!("  {}  ", "x".repeat(400));
        let reason = health
            .slicer_exited(Instant::now(), "exit status: 1", Some(&line))
            .unwrap();
        let (_, appended) = reason.rsplit_once(": ").unwrap();
        assert_eq!(appended.chars().count(), MAX_STDERR_REASON_CHARS);
        assert!(!appended.contains(' '), "trimmed of surrounding blanks");
    }

    #[test]
    fn after_confirmation_the_third_exit_in_ten_minutes_falls_back() {
        let mut health = DirectHealth::default();
        assert!(!health.observe(&stats(Some("wayland"), 60.0)));
        assert!(health.observe(&stats(Some("vulkan-display"), 60.0)));
        assert!(
            !health.observe(&stats(Some("vulkan-display"), 60.0)),
            "only once"
        );
        let start = Instant::now();
        assert_eq!(health.slicer_exited(start, "signal: 9", None), None);
        assert_eq!(
            health.slicer_exited(start + Duration::from_secs(20), "signal: 9", None),
            None
        );
        let reason = health
            .slicer_exited(start + Duration::from_secs(40), "signal: 9 (SIGKILL)", None)
            .expect("third exit");
        assert!(reason.contains("3 times within 10 minutes"), "{reason}");
        assert!(reason.contains("SIGKILL"), "{reason}");
    }

    #[test]
    fn the_runtime_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = RuntimeState::new(dir.path().join("suede"));
        assert_eq!(runtime.session(), None);
        assert_eq!(runtime.fallback_for_boot(Some("boot-a")), None);

        std::fs::create_dir_all(dir.path().join("suede")).unwrap();
        std::fs::write(dir.path().join("suede/session"), "direct\n").unwrap();
        assert_eq!(runtime.session(), Some(SessionKind::Direct));
        std::fs::write(dir.path().join("suede/session"), "wayland\n").unwrap();
        assert_eq!(runtime.session(), Some(SessionKind::Wayland));

        // The profile's own marker shape.
        std::fs::write(
            dir.path().join("suede/presentation-fallback"),
            r#"{"reason":"headless compositor failed to start 3 times","time":1790000000,"bootId":"boot-a"}"#,
        )
        .unwrap();
        let marker = runtime.fallback_for_boot(Some("boot-a")).unwrap();
        assert_eq!(marker.reason, "headless compositor failed to start 3 times");
        assert_eq!(
            runtime.fallback_for_boot(Some("boot-b")),
            None,
            "another boot's"
        );
        // An unreadable marker still counts, as it does for the profile.
        std::fs::write(dir.path().join("suede/presentation-fallback"), "garbage").unwrap();
        assert!(runtime
            .fallback_for_boot(Some("boot-a"))
            .unwrap()
            .reason
            .contains("unreadable"));

        std::fs::write(dir.path().join("suede/direct-attempts"), "2\n").unwrap();
        runtime.clear_direct_attempts().unwrap();
        assert!(!dir.path().join("suede/direct-attempts").exists());
        runtime.clear_direct_attempts().unwrap();
    }

    #[test]
    fn the_first_fallback_reason_wins() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = RuntimeState::new(dir.path().join("suede"));
        runtime.write_fallback("first").unwrap();
        runtime.write_fallback("second").unwrap();
        assert_eq!(runtime.fallback().unwrap().reason, "first");
    }

    #[tokio::test]
    async fn a_session_falls_back_once_and_writes_the_marker_first() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = RuntimeState::new(dir.path().join("suede"));
        let session = DirectSession::new(Some(runtime.clone()));
        assert!(!session.exit_requested());
        session.slicer_exited(
            "exit status: 101",
            Some(
                "direct presentation needs VK_EXT_present_timing and VK_KHR_present_id2; \
                  this device/driver (NVIDIA RTX A1000, 550.163.01) exposes neither",
            ),
        );
        assert!(session.exit_requested());
        let reason = runtime.fallback().unwrap().reason;
        assert!(reason.contains("exit status: 101"), "{reason}");
        assert!(reason.contains("exposes neither"), "{reason}");
        // A later compositor loss does not replace the fallback.
        session.compositor_lost("gone");
        match session.wait_for_exit().await {
            SessionExit::Fallback(reason) => assert!(reason.contains("101")),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn confirmation_clears_the_start_attempts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("suede")).unwrap();
        std::fs::write(dir.path().join("suede/direct-attempts"), "1\n").unwrap();
        let session = DirectSession::new(Some(RuntimeState::new(dir.path().join("suede"))));
        assert!(session.observe_stats(&stats(Some("vulkan-display"), 60.0)));
        assert!(!dir.path().join("suede/direct-attempts").exists());
        // Confirmed: an exit now respawns instead of falling back.
        session.slicer_exited("signal: 9", None);
        assert!(!session.exit_requested());
    }

    #[tokio::test]
    async fn a_compositor_loss_is_an_exit_without_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = RuntimeState::new(dir.path().join("suede"));
        let session = DirectSession::new(Some(runtime.clone()));
        session.compositor_lost("gone");
        assert_eq!(
            session.wait_for_exit().await,
            SessionExit::CompositorLost("gone".to_string())
        );
        assert!(runtime.fallback().is_none());
        // And nothing may fall back (or start a slicer) after it.
        session.fall_back("late");
        assert!(runtime.fallback().is_none());
    }
}

#[cfg(all(test, feature = "projection"))]
mod tests {
    use super::*;
    use crate::drm_inventory::test_support::physical;
    use crate::drm_inventory::{DrmInventory, InventoryOutput};
    use crate::model::{Rect, Renderer};
    use crate::projection::blend::SliceSpec;
    use crate::projection::SlicerSpec;

    fn inventory(outputs: Vec<InventoryOutput>) -> DrmInventory {
        DrmInventory {
            outputs,
            pnp_source: None,
        }
    }

    fn spec(order: &[&str]) -> SlicerSpec {
        SlicerSpec {
            adaptive_lift: None,
            layout: None,
            coverage_rects: Vec::new(),
            highlight_overlaps: false,
            control_session: String::new(),
            source: "HEADLESS-1".to_string(),
            canvas_width: 1920 * order.len() as i32,
            canvas_height: 1080,
            gamma: 2.2,
            black_lift: 0.0,
            pattern: None,
            free_run: false,
            renderer: Renderer::Auto,
            slices: order
                .iter()
                .enumerate()
                .map(|(index, name)| SliceSpec {
                    output: name.to_string(),
                    slice: Rect {
                        x: 1920 * index as i32,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                    source_rect: None,
                    geometry: None,
                })
                .collect(),
        }
    }

    fn observed(inventory: &DrmInventory, refresh_hz: f64) -> Vec<crate::model::Output> {
        inventory
            .simulated_outputs()
            .into_iter()
            .map(|mut output| {
                output.current_mode = Some(crate::model::Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz,
                });
                output
            })
            .collect()
    }

    #[test]
    fn outputs_follow_slice_order_not_inventory_order() {
        let inventory = inventory(vec![
            physical("DP-1", "/dev/dri/card1", Some(129)),
            physical("DP-2", "/dev/dri/card1", Some(133)),
            physical("DP-3", "/dev/dri/card1", Some(137)),
        ]);
        let outputs = observed(&inventory, 59.939);
        let config = direct_config_for(&spec(&["DP-3", "DP-1", "DP-2"]), &outputs, &inventory)
            .expect("derivable");
        assert_eq!(config.card, PathBuf::from("/dev/dri/card1"));
        assert_eq!(
            config
                .outputs
                .iter()
                .map(|o| (o.name.as_str(), o.connector_id))
                .collect::<Vec<_>>(),
            [("DP-3", 137), ("DP-1", 129), ("DP-2", 133)]
        );
        for output in &config.outputs {
            assert_eq!((output.width, output.height), (1920, 1080));
            // Sway's rounded 59.939 Hz, recovered to the exact mode.
            assert_eq!(output.refresh_millihz, 59_939);
        }
    }

    #[test]
    fn a_single_output_is_a_valid_direct_wall() {
        let inventory = inventory(vec![physical("DP-1", "/dev/dri/card1", Some(129))]);
        let outputs = observed(&inventory, 60.0);
        let config = direct_config_for(&spec(&["DP-1"]), &outputs, &inventory).unwrap();
        assert_eq!(config.outputs.len(), 1);
        assert_eq!(config.outputs[0].refresh_millihz, 60_000);
    }

    #[test]
    fn outputs_on_two_cards_are_refused() {
        let inventory = inventory(vec![
            physical("DP-1", "/dev/dri/card0", Some(129)),
            physical("DP-2", "/dev/dri/card1", Some(133)),
        ]);
        let outputs = observed(&inventory, 60.0);
        let error = direct_config_for(&spec(&["DP-1", "DP-2"]), &outputs, &inventory).unwrap_err();
        assert!(error.contains("more than one card"), "{error}");
    }

    #[test]
    fn a_missing_connector_id_or_edid_is_refused() {
        let inventory_without_id = inventory(vec![
            physical("DP-1", "/dev/dri/card1", Some(129)),
            physical("DP-2", "/dev/dri/card1", None),
        ]);
        let outputs = observed(&inventory_without_id, 60.0);
        let error = direct_config_for(&spec(&["DP-1"]), &outputs, &inventory_without_id)
            .expect_err("preflight covers every connected output, not only sliced ones");
        assert!(
            error.contains("DP-2") && error.contains("connector_id"),
            "{error}"
        );

        let mut without_edid = physical("DP-2", "/dev/dri/card1", Some(133));
        without_edid.edid_bytes = 0;
        without_edid.identity = None;
        let inventory_without_edid = inventory(vec![
            physical("DP-1", "/dev/dri/card1", Some(129)),
            without_edid,
        ]);
        let outputs = observed(&inventory_without_edid, 60.0);
        let error = direct_config_for(&spec(&["DP-1", "DP-2"]), &outputs, &inventory_without_edid)
            .unwrap_err();
        assert!(error.contains("DP-2") && error.contains("EDID"), "{error}");
    }

    #[test]
    fn unknown_disabled_or_unadvertised_outputs_are_refused() {
        let inventory = inventory(vec![physical("DP-1", "/dev/dri/card1", Some(129))]);
        let outputs = observed(&inventory, 60.0);
        assert!(direct_config_for(&spec(&["DP-9"]), &outputs, &inventory)
            .unwrap_err()
            .contains("DP-9"));

        let mut disabled = outputs.clone();
        disabled[0].active = false;
        assert!(direct_config_for(&spec(&["DP-1"]), &disabled, &inventory)
            .unwrap_err()
            .contains("not enabled"));

        let odd_rate = observed(&inventory, 59.94);
        assert!(direct_config_for(&spec(&["DP-1"]), &odd_rate, &inventory)
            .unwrap_err()
            .contains("does not advertise"));
    }

    #[test]
    fn the_cpu_renderer_cannot_present_directly() {
        let inventory = inventory(vec![physical("DP-1", "/dev/dri/card1", Some(129))]);
        let outputs = observed(&inventory, 60.0);
        let mut cpu = spec(&["DP-1"]);
        cpu.renderer = Renderer::Cpu;
        assert!(direct_config_for(&cpu, &outputs, &inventory)
            .unwrap_err()
            .contains("GPU renderer"));
    }
}
