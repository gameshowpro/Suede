//! Suede daemon entry point.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use tokio::sync::watch;

use suede::api::{self, ApiState};
use suede::audio::{mock::MockAudio, pw::PipeWireMonitor, AudioMonitor};
use suede::checks::CheckRunner;
use suede::config::BootstrapConfig;
use suede::events::EventHub;
use suede::reconciler::{Reconciler, ReconcilerDeps};
use suede::snapshot::Snapshot;
use suede::state::StateStore;
use suede::supervisor::{LaunchContext, Supervisor};
use suede::sway::{mock::MockSway, SwayClient};

/// How often the environment health checks are re-evaluated.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Parser)]
#[command(
    name = "suede",
    version,
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
    let cli = Cli::parse();

    match cli.command {
        // Kept off the async runtime and away from the logger, so stdout holds
        // nothing but the document.
        Some(Command::Openapi) => {
            println!("{}", api::docs::openapi_document());
            std::process::ExitCode::SUCCESS
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
        version = suede::VERSION,
        bind = %bootstrap.bind,
        state_dir = %bootstrap.state_dir.display(),
        mock = args.mock,
        "starting suede"
    );
    bootstrap.log_security_posture();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let events = EventHub::new();
    let snapshot = Arc::new(Snapshot::new());
    let store = Arc::new(StateStore::load(bootstrap.state_dir.clone())?);

    // --- backends ---
    let sway: Arc<dyn SwayClient> = if args.mock {
        tracing::warn!("running with a mock compositor; no real displays will change");
        Arc::new(MockSway::with_fixtures())
    } else {
        suede::sway::connect(shutdown_rx.clone(), None).await?
    };

    let audio: Arc<dyn AudioMonitor> = if args.mock {
        Arc::new(MockAudio::with_sinks())
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
    ));
    let wallpapers = Arc::new(suede::wallpapers::WallpaperStore::new(
        bootstrap.state_dir.join("wallpapers"),
    ));
    let reconciler = Arc::new(Reconciler::new(ReconcilerDeps {
        sway: sway.clone(),
        audio: audio.clone(),
        store: store.clone(),
        snapshot: snapshot.clone(),
        supervisor: supervisor.clone(),
        events: events.clone(),
        wallpapers: wallpapers.clone(),
        docs_base_url: bootstrap.docs_base_url.clone(),
    }));
    let checks = Arc::new(CheckRunner::new(
        bootstrap.clone(),
        sway.clone(),
        audio.clone(),
        store.clone(),
        events.clone(),
    ));
    let (trigger, trigger_rx) = Reconciler::channel();

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
    tokio::spawn(run_checks(checks.clone(), shutdown_rx.clone()));

    // --- server ---
    let state = ApiState {
        bootstrap: bootstrap.clone(),
        store,
        snapshot,
        events,
        sway,
        audio,
        supervisor: supervisor.clone(),
        reconciler,
        trigger,
        checks,
        wallpapers,
        started_at: Instant::now(),
    };

    let listener = tokio::net::TcpListener::bind(bootstrap.bind).await?;
    tracing::info!(address = %listener.local_addr()?, "listening");

    let server = axum::serve(
        listener,
        // Connection info is what lets the heartbeat endpoint be loopback-only.
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        wait_for_signal().await;
        tracing::info!("shutdown requested");
        let _ = shutdown_tx.send(true);
    });

    server.await?;

    // Terminate managed apps before exiting, so no orphan browsers linger.
    tracing::info!("stopping managed applications");
    supervisor.shutdown().await;
    tracing::info!("suede stopped");
    Ok(())
}

async fn run_checks(checks: Arc<CheckRunner>, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = interval.tick() => {
                checks.run_all().await;
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
