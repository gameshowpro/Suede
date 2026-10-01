//! Everything that talks to Sway.
//!
//! All compositor interaction goes through the [`SwayClient`] trait so the
//! reconciler and API can be tested without a running compositor.

pub mod protocol;
pub mod raw;

#[cfg(unix)]
pub mod client;
pub mod direct;
pub mod mock;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::model::{Output, Window};

#[derive(Debug, thiserror::Error)]
pub enum SwayError {
    #[error("sway IPC socket not found")]
    SocketNotFound,
    #[error("sway IPC protocol error: {0}")]
    Protocol(#[from] protocol::ProtocolError),
    #[error("failed to parse sway response: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("sway rejected command {command:?}: {error}")]
    CommandFailed { command: String, error: String },
    #[error("unexpected reply type {got} (expected {expected})")]
    UnexpectedReply { got: u32, expected: u32 },
    #[error("sway IPC is not available on this platform")]
    Unsupported,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type SwayResult<T> = Result<T, SwayError>;

/// Sway's reported version, and the feature gates derived from it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwayVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub human_readable: Option<String>,
}

impl SwayVersion {
    /// `output … tearing` landed in Sway 1.10.
    pub fn supports_tearing(&self) -> bool {
        (self.major, self.minor) >= (1, 10)
    }

    pub fn display(&self) -> String {
        self.human_readable
            .clone()
            .unwrap_or_else(|| format!("{}.{}.{}", self.major, self.minor, self.patch))
    }
}

/// Something observed on the subscribed IPC connection.
#[derive(Debug, Clone)]
pub enum SwayEvent {
    /// Sway's `output` event carries no detail, so it is only a trigger to re-query.
    OutputsMayHaveChanged,
    /// A window appeared, vanished, or changed.
    Window { change: String, window: Window },
    /// Sway is going away.
    Shutdown,
}

/// The compositor operations Suede needs.
#[async_trait]
pub trait SwayClient: Send + Sync + 'static {
    /// Current outputs, as reported by `get_outputs`.
    async fn get_outputs(&self) -> SwayResult<Vec<Output>>;

    /// Real windows in the tree, with the output each sits on.
    async fn get_windows(&self) -> SwayResult<Vec<Window>>;

    /// Run a single Sway command, failing if Sway reports it unsuccessful.
    async fn run_command(&self, command: &str) -> SwayResult<()>;

    /// Run several commands as sway would see one IPC message, one result
    /// per command in order. A transport failure (the connection itself, not
    /// a single command sway rejects) is reported for every entry, since
    /// nothing about which of them landed is knowable in that case.
    ///
    /// The default loops over [`run_command`](Self::run_command) — correct,
    /// just not what this exists for: `IpcClient` overrides it to send every
    /// command in one sway IPC message, which is what lets several output
    /// commands land in a single compositor backend commit. See the
    /// reconciler's call site for the measurement that makes that matter.
    async fn run_commands(&self, commands: &[String]) -> Vec<SwayResult<()>> {
        let mut results = Vec::with_capacity(commands.len());
        for command in commands {
            results.push(self.run_command(command).await);
        }
        results
    }

    /// Sway's version, cached after the first successful query.
    async fn get_version(&self) -> SwayResult<SwayVersion>;

    /// Receiver for compositor events.
    fn subscribe(&self) -> broadcast::Receiver<SwayEvent>;

    /// Whether the event connection is currently established.
    fn is_connected(&self) -> bool;
}

/// Locate the Sway IPC socket, in the order sway itself documents.
pub fn discover_socket() -> Option<PathBuf> {
    // 1. The environment variable, when Suede shares the compositor's session.
    if let Ok(path) = std::env::var("SWAYSOCK") {
        let path = PathBuf::from(path);
        if path.exists() && socket_owner_alive(&path) {
            return Some(path);
        }
    }

    // 2. The session's runtime directory.
    if let Some(dir) = crate::util::runtime_dir() {
        if let Some(found) = scan_for_socket(&dir) {
            return Some(found);
        }
    }

    // 3. Any user's runtime directory, for a daemon started outside the session.
    if let Ok(entries) = std::fs::read_dir("/run/user") {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| scan_for_socket(&entry.path()))
            .collect();
        candidates.sort();
        if let Some(found) = candidates.into_iter().next() {
            return Some(found);
        }
    }

    None
}

fn scan_for_socket(dir: &std::path::Path) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("sway-ipc."))
        })
        .filter(|path| socket_owner_alive(path))
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// Whether the sway that created `path` is still running, as far as can be
/// told: sway names its socket `sway-ipc.<uid>.<pid>.sock`, and a sway that
/// was killed rather than asked to exit leaves the file behind. Connecting
/// to such a leftover can only fail, and a daemon that picked one would stay
/// bound to it, so it is skipped. Anything that cannot be judged — a name in
/// another shape, a system without `/proc` — counts as alive, which is what
/// discovery assumed before.
///
/// Matters most around a session switch (experimental direct presentation's
/// fallback), where the daemon restarts while the old compositor's socket is
/// gone and the new one's is not yet in systemd's environment.
pub fn socket_owner_alive(path: &std::path::Path) -> bool {
    let Some(pid) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("sway-ipc."))
        .and_then(|rest| rest.strip_suffix(".sock"))
        .and_then(|rest| rest.split('.').nth(1))
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return true;
    };
    let proc = std::path::Path::new("/proc");
    !proc.join("self").exists() || proc.join(pid.to_string()).exists()
}

/// Connect to Sway and start pumping its events.
///
/// Suede is started by systemd alongside the session, so the socket may not
/// exist yet; this waits for it with backoff rather than failing at startup.
pub async fn connect(
    shutdown: tokio::sync::watch::Receiver<bool>,
    deadline: Option<std::time::Duration>,
) -> SwayResult<Arc<dyn SwayClient>> {
    connect_with_path(shutdown, deadline)
        .await
        .map(|(client, _)| client)
}

/// [`connect`], also returning the socket path the client is bound to — what
/// experimental direct presentation watches to learn that its compositor
/// has gone away.
pub async fn connect_with_path(
    shutdown: tokio::sync::watch::Receiver<bool>,
    deadline: Option<std::time::Duration>,
) -> SwayResult<(Arc<dyn SwayClient>, PathBuf)> {
    #[cfg(not(unix))]
    {
        let _ = (shutdown, deadline);
        Err(SwayError::Unsupported)
    }

    #[cfg(unix)]
    {
        let started = std::time::Instant::now();
        let mut delay = std::time::Duration::from_millis(250);
        loop {
            if let Some(path) = discover_socket() {
                tracing::info!(socket = %path.display(), "using sway IPC socket");
                let client = Arc::new(client::IpcClient::new(path.clone()));
                tokio::spawn(client.clone().run_event_loop(shutdown));
                return Ok((client, path));
            }
            if let Some(deadline) = deadline {
                if started.elapsed() >= deadline {
                    return Err(SwayError::SocketNotFound);
                }
            }
            tracing::info!("waiting for sway IPC socket");
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tearing_support_is_gated_on_1_10() {
        let old = SwayVersion {
            major: 1,
            minor: 9,
            patch: 0,
            human_readable: None,
        };
        let new = SwayVersion {
            major: 1,
            minor: 10,
            patch: 1,
            human_readable: None,
        };
        assert!(!old.supports_tearing());
        assert!(new.supports_tearing());
    }

    #[test]
    fn finds_socket_in_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Named for a process that exists (this one), since a socket whose
        // sway has died is skipped — see the test below.
        std::fs::write(
            dir.path()
                .join(format!("sway-ipc.1000.{}.sock", std::process::id())),
            b"",
        )
        .unwrap();
        std::fs::write(dir.path().join("unrelated"), b"").unwrap();
        let found = scan_for_socket(dir.path()).unwrap();
        assert!(found
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("sway-ipc."));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_socket_left_behind_by_a_dead_sway_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // Above the kernel's largest possible pid (2^22), so never running.
        let dead = dir.path().join("sway-ipc.1000.99999999.sock");
        std::fs::write(&dead, b"").unwrap();
        assert!(!socket_owner_alive(&dead));
        assert!(scan_for_socket(dir.path()).is_none());

        let live = dir
            .path()
            .join(format!("sway-ipc.1000.{}.sock", std::process::id()));
        std::fs::write(&live, b"").unwrap();
        assert_eq!(scan_for_socket(dir.path()), Some(live));
        // A name in some other shape cannot be judged, so it is kept.
        assert!(socket_owner_alive(std::path::Path::new("sway-ipc.sock")));
    }

    #[test]
    fn no_socket_in_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_for_socket(dir.path()).is_none());
    }
}
