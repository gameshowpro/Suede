//! Suede daemon entry point.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use tokio::sync::watch;

use suede::api::{self, ApiState};
use suede::audio::{mock::MockAudio, pw::PipeWireMonitor, AudioMonitor};
use suede::checks::{CheckRunner, CheckRunnerDeps};
use suede::config::BootstrapConfig;
use suede::events::EventHub;
use suede::reconciler::{Reconciler, ReconcilerDeps};
use suede::snapshot::Snapshot;
use suede::state::StateStore;
use suede::supervisor::{LaunchContext, Supervisor};
use suede::sway::{mock::MockSway, SwayClient};

/// How often the environment health checks are re-evaluated.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// How long to let a configuration write settle before re-running the checks.
const CHECK_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Parser)]
#[command(
    name = "suede",
    // The build identity, not just the release number: "0.1.0" cannot
    // distinguish the build that has a fix in it from the one that does not.
    version = suede::VERSION_STRING,
    about = "Remote management daemon for Sway-based display appliances"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the bootstrap configuration file.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon. This is the default.
    Run(RunArgs),
    /// Print the OpenAPI document to stdout and exit.
    ///
    /// Needs neither sway nor a network, so CI can build the published API
    /// reference from the exact code being released.
    Openapi,
    /// Internal: draw edge-blend ramps on one output. Spawned by the daemon.
    #[cfg(all(feature = "projection", unix))]
    #[command(hide = true)]
    Blend(BlendArgs),
    /// Internal: capture the canvas and present per-projector slices.
    #[cfg(all(feature = "projection", unix))]
    #[command(hide = true)]
    Slice(SliceArgs),
    /// Clear a stale NVKMS sub-ownership grant on every DRM card.
    ///
    /// Safe to run whenever, including while another process holds DRM
    /// master: the revoke then simply fails on each card that is not
    /// grantable, which is reported and not treated as an error. Used by the
    /// login profile before starting the DRM Sway after a direct-mode
    /// session, and for manual recovery after a crashed slicer.
    #[cfg(all(feature = "projection", unix))]
    DisplayReset,
    /// Diagnostic: print the live DRM inventory direct presentation would
    /// use — each connector's EDID identity, connector id and modes, read
    /// from sysfs and a read-only GETCONNECTOR — and the outputs it would
    /// report in Sway's place, as JSON.
    #[command(hide = true)]
    DrmInventory,
    /// Diagnostic: print the live NVIDIA driver and GSP firmware state
    /// `GET /system`'s `nvidiaDriver` reports, read straight from
    /// `/proc/driver/nvidia`, as JSON (`null` on a non-NVIDIA machine).
    #[command(hide = true)]
    NvidiaDriver,
}

#[cfg(all(feature = "projection", unix))]
#[derive(Args)]
struct BlendArgs {
    /// The overlay specification, as serialized by the daemon.
    #[arg(long, value_name = "JSON")]
    spec: String,
}

#[cfg(all(feature = "projection", unix))]
#[derive(Args)]
struct SliceArgs {
    /// The overlay specification, as serialized by the daemon.
    #[arg(long, value_name = "JSON")]
    spec: String,

    /// Experimental direct-display presentation configuration, from a file.
    #[arg(long, value_name = "FILE", conflicts_with = "presentation_config_json")]
    presentation_config: Option<PathBuf>,

    /// Experimental direct-display presentation configuration, inline. An
    /// alternative to `--presentation-config` for callers that already hold
    /// the JSON in memory and would rather not stage a temporary file.
    #[arg(long, value_name = "JSON")]
    presentation_config_json: Option<String>,

    /// End a bounded renderer test through normal resource cleanup.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
    run_for_seconds: Option<u64>,
}

#[derive(Args, Default)]
struct RunArgs {
    /// Override the bind address.
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,

    /// Run against in-memory mocks, for developing without sway or PipeWire.
    #[arg(long)]
    mock: bool,
}

fn main() -> std::process::ExitCode {
    // Anchors the stamp in the emitted file. `#[used]` alone protects it
    // from the compiler but not from the linker's section GC, and the cross
    // container's older toolchain discarded it that way - locally linked
    // binaries kept it, CI's lost it, and the identity check can only grep
    // what is actually there. An opaque use is the one thing every layer
    // must preserve.
    std::hint::black_box(suede::BUILD_STAMP);

    let cli = Cli::parse();

    match cli.command {
        // Kept off the async runtime and away from the logger, so stdout holds
        // nothing but the document.
        Some(Command::Openapi) => {
            println!("{}", api::docs::openapi_document());
            std::process::ExitCode::SUCCESS
        }
        // Synchronous and runtime-free: one Wayland socket, one static image.
        #[cfg(all(feature = "projection", unix))]
        Some(Command::Blend(args)) => {
            let spec = match serde_json::from_str(&args.spec) {
                Ok(spec) => spec,
                Err(error) => {
                    eprintln!("invalid --spec: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match suede::projection::overlay::run(&spec) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("blend overlay failed: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        #[cfg(all(feature = "projection", unix))]
        Some(Command::Slice(args)) => {
            let spec = match serde_json::from_str(&args.spec) {
                Ok(spec) => spec,
                Err(error) => {
                    eprintln!("invalid --spec: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let presentation_config =
                match (args.presentation_config, args.presentation_config_json) {
                    (Some(path), _) => {
                        let contents = match std::fs::read_to_string(&path) {
                            Ok(contents) => contents,
                            Err(error) => {
                                eprintln!(
                                    "failed to read --presentation-config {}: {error}",
                                    path.display()
                                );
                                return std::process::ExitCode::FAILURE;
                            }
                        };
                        match serde_json::from_str::<
                            suede::projection::gpu::display::DirectDisplayConfig,
                        >(&contents)
                        {
                            Ok(config) => Some(config),
                            Err(error) => {
                                eprintln!(
                                    "invalid --presentation-config {}: {error}",
                                    path.display()
                                );
                                return std::process::ExitCode::FAILURE;
                            }
                        }
                    }
                    // `conflicts_with` on the arg definitions already rules out
                    // both flags at once, so this is the file-less path.
                    (None, Some(json)) => {
                        match serde_json::from_str::<
                            suede::projection::gpu::display::DirectDisplayConfig,
                        >(&json)
                        {
                            Ok(config) => Some(config),
                            Err(error) => {
                                eprintln!("invalid --presentation-config-json: {error}");
                                return std::process::ExitCode::FAILURE;
                            }
                        }
                    }
                    (None, None) => None,
                };
            let deadline = args
                .run_for_seconds
                .map(|seconds| std::time::Instant::now() + std::time::Duration::from_secs(seconds));
            let result = suede::projection::slicer::run_with_presentation_until(
                &spec,
                presentation_config.as_ref(),
                deadline,
            );
            match result {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("slicer failed: {error:#}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        #[cfg(all(feature = "projection", unix))]
        Some(Command::DisplayReset) => {
            suede::projection::gpu::display::display_reset();
            std::process::ExitCode::SUCCESS
        }
        Some(Command::DrmInventory) => {
            let inventory = suede::drm_inventory::DrmInventory::read();
            let report = serde_json::json!({
                "preflight": match inventory.preflight() {
                    Ok(card) => serde_json::json!({ "card": card }),
                    Err(error) => serde_json::json!({ "refused": error }),
                },
                "outputs": inventory.simulated_outputs(),
                "inventory": inventory,
            });
            match serde_json::to_string_pretty(&report) {
                Ok(text) => {
                    println!("{text}");
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("could not encode the inventory: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Some(Command::NvidiaDriver) => {
            let status = suede::nvidia_driver::detect();
            match serde_json::to_string_pretty(&status) {
                Ok(text) => {
                    println!("{text}");
                    std::process::ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("could not encode the NVIDIA driver status: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Some(Command::Run(args)) => run(cli.config, args),
        None => run(cli.config, RunArgs::default()),
    }
}

fn run(config_path: Option<PathBuf>, args: RunArgs) -> std::process::ExitCode {
    init_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start the async runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(serve(config_path, args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "suede exited with an error");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn serve(config_path: Option<PathBuf>, args: RunArgs) -> anyhow::Result<()> {
    let mut bootstrap = BootstrapConfig::load(config_path.as_deref())?;
    if let Some(bind) = args.bind {
        bootstrap.bind = bind.parse()?;
    }
    let bootstrap = Arc::new(bootstrap);

    tracing::info!(
        version = suede::VERSION_STRING,
        bind = %bootstrap.bind,
        state_dir = %bootstrap.state_dir.display(),
        allow_overlaps = bootstrap.allow_overlaps,
        direct_scanout = bootstrap.direct_scanout,
        presentation = bootstrap.presentation.as_str(),
        align_outputs = bootstrap.align_outputs,
        restart_on_gpu_fallback = bootstrap.restart_on_gpu_fallback,
        mock = args.mock,
        "starting suede"
    );
    if bootstrap.presentation == suede::model::PresentationMode::Direct {
        tracing::warn!("presentation = \"direct\" is experimental");
    }
    bootstrap.log_security_posture();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let events = EventHub::new();
    let snapshot = Arc::new(Snapshot::new());
    let store = Arc::new(StateStore::load(bootstrap.state_dir.clone())?);

    // A document repaired at load is a daemon-side change to the effective
    // document exactly like a commit or a revert, so it is published the
    // same way — here, the first place anything after `load` consults the
    // store, and the only time `load_repairs` is non-empty (it clears once
    // the repaired document has been saved once). No SSE client is likely to
    // be connected this early, so this is a best-effort courtesy for one
    // already watching across a restart; `/status` (`state_document_repaired`)
    // is the durable way an operator finds out.
    if !store.load_repairs().is_empty() {
        let (document, version) = store.effective_with_version();
        events.publish(suede::events::ServerEvent::ConfigChanged(Box::new(
            api::config_change("repair", &document, &version),
        )));
    }

    // --- backends ---
    let (sway, sway_socket): (Arc<dyn SwayClient>, Option<PathBuf>) = if args.mock {
        tracing::warn!("running with a mock compositor; no real displays will change");
        (Arc::new(MockSway::with_fixtures()), None)
    } else {
        let (client, socket) = suede::sway::connect_with_path(shutdown_rx.clone(), None).await?;
        (client, Some(socket))
    };

    // Experimental direct presentation: decide what this session does,
    // against the compositor the login actually started. The displays are
    // then not the compositor's, so they are read from the kernel and the
    // daemon stands in for Sway on them.
    let direct_inventory = match &sway_socket {
        Some(socket) => match resolve_presentation(&bootstrap, &sway, &snapshot).await? {
            PresentationStart::Wayland => None,
            PresentationStart::Direct(inventory) => Some(inventory),
            PresentationStart::EndCompositor => {
                // Nothing presents yet, so there is nothing else to stop.
                suede::presentation::end_headless_compositor(sway.as_ref(), socket).await;
                tracing::info!("suede stopped; the next session is resolved afresh");
                return Ok(());
            }
        },
        None => None,
    };
    let direct_session = direct_inventory.as_ref().map(|_| {
        suede::presentation::DirectSession::new(suede::presentation::RuntimeState::from_env())
    });
    // The compositor itself, for what only it can do: be watched, and be
    // told to exit. Everything else goes through `sway` below.
    let compositor = sway.clone();
    let sway: Arc<dyn SwayClient> = match &direct_inventory {
        Some(inventory) => suede::sway::direct::DirectOutputs::new(sway, inventory),
        None => sway,
    };

    let audio: Arc<dyn AudioMonitor> = if args.mock {
        Arc::new(MockAudio::with_devices())
    } else {
        let monitor = Arc::new(PipeWireMonitor::new());
        tokio::spawn(monitor.clone().run(shutdown_rx.clone()));
        monitor
    };

    // --- core ---
    let supervisor = Arc::new(Supervisor::new(
        sway.clone(),
        events.clone(),
        LaunchContext {
            profiles_root: bootstrap.state_dir.join("profiles"),
            log_root: bootstrap.state_dir.join("logs"),
            // Loopback: the browsers posting heartbeats run on this machine.
            api_base: format!("http://127.0.0.1:{}/api/v1", bootstrap.bind.port()),
        },
        bootstrap.allowed_programs.clone(),
    ));
    let wallpapers = Arc::new(suede::wallpapers::WallpaperStore::new(
        bootstrap.state_dir.join("wallpapers"),
    ));
    let reconciler = Arc::new(
        Reconciler::new(ReconcilerDeps {
            sway: sway.clone(),
            audio: audio.clone(),
            store: store.clone(),
            snapshot: snapshot.clone(),
            supervisor: supervisor.clone(),
            events: events.clone(),
            wallpapers: wallpapers.clone(),
            docs_base_url: bootstrap.docs_base_url.clone(),
            allow_overlaps: bootstrap.allow_overlaps,
        })
        .with_direct_presentation(direct_inventory)
        .with_direct_session(direct_session.clone()),
    );
    let capability_store = Arc::new(suede::capabilities::CapabilityStore::new(
        &bootstrap.state_dir,
    ));
    let (trigger, trigger_rx) = Reconciler::channel();
    let checks = Arc::new(CheckRunner::new(CheckRunnerDeps {
        bootstrap: bootstrap.clone(),
        sway: sway.clone(),
        audio: audio.clone(),
        store: store.clone(),
        events: events.clone(),
        capabilities: capability_store.clone(),
        snapshot: snapshot.clone(),
        trigger: trigger.clone(),
        supervisor: supervisor.clone(),
    }));

    // --- background tasks ---
    tokio::spawn(Reconciler::forward_sway_events(
        sway.clone(),
        events.clone(),
        trigger.clone(),
        shutdown_rx.clone(),
    ));
    tokio::spawn(Reconciler::forward_audio_events(
        audio.clone(),
        events.clone(),
        trigger.clone(),
        shutdown_rx.clone(),
    ));
    tokio::spawn(reconciler.clone().run(trigger_rx, shutdown_rx.clone()));
    tokio::spawn(
        suede::alignment::Aligner::new(
            bootstrap.align_outputs,
            checks.clone(),
            snapshot.clone(),
            trigger.clone(),
        )
        .run(shutdown_rx.clone()),
    );
    tokio::spawn(
        suede::browser_gpu::Watchdog::new(
            bootstrap.restart_on_gpu_fallback,
            supervisor.clone(),
            checks.clone(),
            snapshot.clone(),
        )
        .run(shutdown_rx.clone()),
    );
    tokio::spawn(run_checks(
        checks.clone(),
        events.clone(),
        shutdown_rx.clone(),
    ));
    // Deliberately a task on this runtime rather than a thread of its own: if
    // the runtime stops turning, this stops reporting, and systemd restarts
    // the daemon. A watchdog that could still tick while the thing it vouches
    // for was dead would be worse than none.
    tokio::spawn(suede::watchdog::feed(shutdown_rx.clone()));
    if let (Some(session), Some(socket)) = (&direct_session, &sway_socket) {
        tokio::spawn(session.clone().watch_stats(
            events.clone(),
            snapshot.clone(),
            shutdown_rx.clone(),
        ));
        tokio::spawn(session.clone().watch_compositor(
            compositor.clone(),
            socket.clone(),
            shutdown_rx.clone(),
        ));
    }

    // --- server ---
    let state = ApiState {
        bootstrap: bootstrap.clone(),
        store,
        snapshot,
        events,
        sway,
        audio,
        supervisor: supervisor.clone(),
        reconciler: reconciler.clone(),
        trigger,
        checks,
        wallpapers,
        capabilities: std::sync::Arc::new(api::capabilities::CapabilityChecks::default()),
        capability_store,
        power: std::sync::Arc::new(api::observed::SystemPower),
        started_at: Instant::now(),
    };

    // Runs once the server below is accepting, and opens a window only when
    // the stored measurement no longer describes this machine.
    tokio::spawn(api::capabilities::boot_measure(state.clone()));

    let listener = tokio::net::TcpListener::bind(bootstrap.bind).await?;
    tracing::info!(address = %listener.local_addr()?, "listening");
    // Once the socket is bound, the daemon is genuinely up. Under
    // `Type=notify` systemd holds dependent units until this arrives, so it
    // goes here rather than at the top of main.
    suede::watchdog::notify_ready();

    let session_for_shutdown = direct_session.clone();
    let server = axum::serve(
        listener,
        // Connection info is what lets the heartbeat endpoint be loopback-only.
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        match &session_for_shutdown {
            Some(session) => tokio::select! {
                _ = wait_for_signal() => tracing::info!("shutdown requested"),
                exit = session.wait_for_exit() => {
                    tracing::warn!(?exit, "the direct session is ending; shutting down");
                }
            },
            None => {
                wait_for_signal().await;
                tracing::info!("shutdown requested");
            }
        }
        let _ = shutdown_tx.send(true);
    });

    // An open SSE/long-poll connection never closes by itself, so axum's own
    // graceful shutdown (started once the future above resolves) would
    // otherwise wait on it indefinitely — observed stretching a plain
    // `systemctl --user restart suede` all the way to systemd's own
    // `TimeoutStopSec` and a SIGKILL, with a stale `/events` stream holding
    // the old daemon in stop-sigterm for the full timeout. Bounding the wait
    // here, rather than trusting the client to disconnect, keeps a session
    // switch's dark gap from depending on whoever happens to be connected —
    // for a direct session ending, that dark gap is the wall itself.
    //
    // The clock starts only once shutdown has actually been requested
    // (`shutdown_tx.send(true)` above, from a signal or a direct session
    // ending), never from server startup: an otherwise-idle daemon with an
    // open event stream must never time out on its own.
    let mut server = std::pin::pin!(std::future::IntoFuture::into_future(server));
    let mut shutdown_signaled = shutdown_rx.clone();
    tokio::select! {
        result = &mut server => result?,
        _ = async {
            while !*shutdown_signaled.borrow() {
                if shutdown_signaled.changed().await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(SHUTDOWN_DRAIN_GRACE).await;
        } => {
            tracing::warn!(
                bound_s = SHUTDOWN_DRAIN_GRACE.as_secs_f64(),
                "not waiting any longer for open connections to close"
            );
        }
    }

    // Terminate managed apps before exiting, so no orphan browsers linger.
    tracing::info!("stopping managed applications");
    supervisor.shutdown().await;
    // Same rule for blend overlays: nothing spawned outlives the daemon. A
    // direct slicer is stopped gracefully here, handing the displays back.
    reconciler.shutdown().await;
    match direct_session
        .as_ref()
        .and_then(|session| session.exit_reason())
    {
        Some(suede::presentation::SessionExit::Fallback(_)) => {
            if let Some(socket) = &sway_socket {
                suede::presentation::end_headless_compositor(compositor.as_ref(), socket).await;
            }
        }
        Some(suede::presentation::SessionExit::CompositorLost(_)) => {
            // The next login may be starting the DRM sway already; clearing
            // a grant a killed slicer left behind is harmless either way.
            suede::presentation::run_display_reset().await;
        }
        None => {}
    }
    tracing::info!("suede stopped");
    Ok(())
}

/// How long the API server's open connections (an SSE/long-poll client never
/// closes on its own) get to drain once shutdown has actually been
/// requested — SIGTERM, Ctrl-C, or a direct session ending — before the
/// daemon stops waiting on them and moves on to shutting the rest of itself
/// down. Systemd's `TimeoutStopSec` is the backstop if this bound is ever
/// missed; staying well under it is what keeps a session switch's dark gap
/// from being stretched by whichever client happened to be connected.
const SHUTDOWN_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// What [`resolve_presentation`] decided.
enum PresentationStart {
    Wayland,
    Direct(Arc<suede::drm_inventory::DrmInventory>),
    /// A headless compositor from a direct session this session must not
    /// continue: end it, and exit, so the login starts the ordinary one.
    EndCompositor,
}

/// Resolve this session's presentation against the compositor the daemon
/// connected to and the login profile's runtime files, record it for
/// `GET /system`, and — for direct — read and preflight the displays.
///
/// An appliance that never asked for direct presentation (and whose last
/// login did not start a direct session) resolves without asking the
/// compositor anything, so it starts exactly as it always has.
async fn resolve_presentation(
    bootstrap: &BootstrapConfig,
    sway: &Arc<dyn SwayClient>,
    snapshot: &Snapshot,
) -> anyhow::Result<PresentationStart> {
    use suede::model::{PresentationMode, PresentationStatus};
    use suede::presentation::{self as presentation, Resolution};

    let runtime = presentation::RuntimeState::from_env();
    let file = bootstrap.presentation_effective();
    let session = runtime.as_ref().and_then(|runtime| runtime.session());
    let headless_only = if presentation::needs_compositor(file.0, session) {
        presentation::headless_only(&compositor_outputs(sway).await?)
    } else {
        false
    };
    let input = presentation::ResolutionInput {
        file,
        fallback: runtime
            .as_ref()
            .and_then(|runtime| runtime.fallback())
            .map(|marker| marker.reason),
        headless_only,
        session,
    };
    let resolution = presentation::resolve(&input);
    tracing::info!(
        ?resolution,
        headless_only,
        ?session,
        "presentation resolved"
    );

    let status = |effective, reason: Option<String>, outputs| PresentationStatus {
        requested: bootstrap.presentation,
        effective,
        reason,
        outputs,
    };
    match resolution {
        Resolution::Wayland { reason } => {
            if let Some(reason) = &reason {
                tracing::warn!(
                    requested = bootstrap.presentation.as_str(),
                    %reason,
                    "presenting through the compositor instead of directly"
                );
            }
            snapshot.set_presentation(status(PresentationMode::Wayland, reason, Vec::new()));
            Ok(PresentationStart::Wayland)
        }
        Resolution::FinishSwitch { reason } => {
            tracing::warn!(%reason, "this compositor was started for direct presentation; ending it");
            Ok(PresentationStart::EndCompositor)
        }
        Resolution::Direct => {
            let inventory = suede::drm_inventory::DrmInventory::read();
            match inventory.preflight() {
                Ok(card) => {
                    tracing::info!(
                        card = %card.display(),
                        outputs = inventory.connected().count(),
                        pnp_names = inventory.pnp_source.as_deref().unwrap_or("none"),
                        "presenting directly (experimental)"
                    );
                    snapshot.set_presentation(status(
                        PresentationMode::Direct,
                        None,
                        inventory
                            .connected()
                            .map(|output| output.name.clone())
                            .collect(),
                    ));
                    Ok(PresentationStart::Direct(Arc::new(inventory)))
                }
                Err(refusal) => {
                    // Never present through the headless compositor: it has
                    // no displays. Fall back for the rest of this boot.
                    let reason = format!("direct presentation preflight refused: {refusal}");
                    let session = presentation::DirectSession::new(runtime);
                    session.fall_back(reason);
                    Ok(PresentationStart::EndCompositor)
                }
            }
        }
    }
}

/// The compositor's outputs, retried briefly: it may still be coming up.
async fn compositor_outputs(
    sway: &Arc<dyn SwayClient>,
) -> anyhow::Result<Vec<suede::model::Output>> {
    let mut attempt = 0;
    loop {
        match sway.get_outputs().await {
            Ok(outputs) => return Ok(outputs),
            Err(error) if attempt < 10 => {
                tracing::info!(%error, "waiting for the compositor to list its outputs");
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn run_checks(
    checks: Arc<CheckRunner>,
    events: suede::events::EventHub,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Several checks are judgements about the configuration - whether the
    // apps that are configured can actually be launched, whether an output
    // asking for a background can get one - so a write can settle or provoke
    // one instantly. On the timer alone the operator fixes the problem and
    // then goes on looking at the warning for up to a minute, which reads as
    // the fix not having worked.
    let mut changes = events.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = interval.tick() => {
                checks.run_all().await;
            }
            event = changes.recv() => {
                match event {
                    Ok(suede::events::ServerEvent::ConfigChanged(_)) => {
                        // Let the write settle, and coalesce a burst: a client
                        // saving several sections sends several. The checks
                        // shell out to other programs, so running them once
                        // per write in a burst is worth avoiding.
                        tokio::time::sleep(CHECK_SETTLE).await;
                        while changes.try_recv().is_ok() {}
                        checks.run_all().await;
                    }
                    // Everything else, including this task's own ChecksChanged.
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Missed events may have included a config write.
                        checks.run_all().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "cannot listen for SIGTERM");
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("suede=info,warn"));

    // Log to the journal only when systemd is actually supervising us — it
    // sets JOURNAL_STREAM for its services. Detecting the journal's mere
    // availability would silently swallow the output of a foreground run,
    // which is exactly when someone is watching the terminal.
    #[cfg(target_os = "linux")]
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        if let Ok(journald) = tracing_journald::layer() {
            tracing_subscriber::registry()
                .with(filter)
                .with(journald)
                .init();
            return;
        }
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
}
