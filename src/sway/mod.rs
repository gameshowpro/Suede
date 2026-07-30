//! Everything that talks to Sway.
//!
//! All compositor interaction goes through the [`SwayClient`] trait so the
//! reconciler and API can be tested without a running compositor.

pub mod protocol;
pub mod raw;

#[cfg(unix)]
pub mod client;
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
        if path.exists() {
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
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// Connect to Sway and start pumping its events.
///
/// Suede is started by systemd alongside the session, so the socket may not
/// exist yet; this waits for it with backoff rather than failing at startup.
pub async fn connect(
    shutdown: tokio::sync::watch::Receiver<bool>,
    deadline: Option<std::time::Duration>,
) -> SwayResult<Arc<dyn SwayClient>> {
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
                let client = Arc::new(client::IpcClient::new(path));
                tokio::spawn(client.clone().run_event_loop(shutdown));
                return Ok(client);
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
        std::fs::write(dir.path().join("sway-ipc.1000.4242.sock"), b"").unwrap();
        std::fs::write(dir.path().join("unrelated"), b"").unwrap();
        let found = scan_for_socket(dir.path()).unwrap();
        assert!(found
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("sway-ipc."));
    }

    #[test]
    fn no_socket_in_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_for_socket(dir.path()).is_none());
    }
}
