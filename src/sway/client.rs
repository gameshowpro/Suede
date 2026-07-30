//! Live Sway IPC client.
//!
//! Queries and commands each use a short-lived connection, which keeps failure
//! handling trivial; a single long-lived connection carries the event
//! subscription and reconnects with backoff.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, watch};

use super::protocol::{self, message, read_message, write_message};
use super::raw::{
    collect_windows, RawCommandOutcome, RawNode, RawOutput, RawVersion, RawWindowEvent,
};
use super::{SwayClient, SwayError, SwayEvent, SwayResult, SwayVersion};
use crate::model::{Output, Window};

const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct IpcClient {
    socket: PathBuf,
    version: RwLock<Option<SwayVersion>>,
    events: broadcast::Sender<SwayEvent>,
    connected: AtomicBool,
}

impl IpcClient {
    pub fn new(socket: PathBuf) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            socket,
            version: RwLock::new(None),
            events,
            connected: AtomicBool::new(false),
        }
    }

    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket
    }

    /// One request/reply exchange on a fresh connection.
    async fn request(&self, message_type: u32, payload: &[u8]) -> SwayResult<Vec<u8>> {
        let mut stream = UnixStream::connect(&self.socket).await?;
        write_message(&mut stream, message_type, payload).await?;
        let (reply_type, body) = read_message(&mut stream).await?;
        if reply_type != message_type {
            return Err(SwayError::UnexpectedReply {
                got: reply_type,
                expected: message_type,
            });
        }
        Ok(body)
    }

    /// Pump compositor events into the broadcast channel until shutdown.
    ///
    /// Sway's `output` event carries no payload of use (`change` is
    /// "unspecified"), so it is forwarded as a bare trigger to re-query.
    pub async fn run_event_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut delay = std::time::Duration::from_millis(250);
        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                result = self.event_session() => {
                    self.connected.store(false, Ordering::Relaxed);
                    match result {
                        Ok(()) => tracing::warn!("sway closed the event connection"),
                        Err(error) => tracing::warn!(%error, "sway event connection failed"),
                    }
                }
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(std::time::Duration::from_secs(5));
        }
    }

    async fn event_session(&self) -> SwayResult<()> {
        let mut stream = UnixStream::connect(&self.socket).await?;
        let payload = br#"["window","output","shutdown"]"#;
        write_message(&mut stream, message::SUBSCRIBE, payload).await?;

        let (reply_type, body) = read_message(&mut stream).await?;
        if reply_type != message::SUBSCRIBE {
            return Err(SwayError::UnexpectedReply {
                got: reply_type,
                expected: message::SUBSCRIBE,
            });
        }
        let outcome: RawCommandOutcome = serde_json::from_slice(&body)?;
        if !outcome.success {
            return Err(SwayError::CommandFailed {
                command: "subscribe".into(),
                error: outcome.error.unwrap_or_else(|| "unknown error".into()),
            });
        }

        self.connected.store(true, Ordering::Relaxed);
        tracing::info!("subscribed to sway events");

        loop {
            let (event_type, body) = read_message(&mut stream).await?;
            match event_type {
                protocol::event::OUTPUT => {
                    let _ = self.events.send(SwayEvent::OutputsMayHaveChanged);
                }
                protocol::event::WINDOW => {
                    if let Some(event) = parse_window_event(&body) {
                        let _ = self.events.send(event);
                    }
                }
                protocol::event::SHUTDOWN => {
                    let _ = self.events.send(SwayEvent::Shutdown);
                    return Ok(());
                }
                other => tracing::trace!(event_type = other, "ignoring sway event"),
            }
        }
    }
}

/// Changes worth reacting to. Anything else is noise for our purposes.
const INTERESTING_CHANGES: [&str; 6] = [
    "new",
    "close",
    "title",
    "move",
    "fullscreen_mode",
    "floating",
];

fn parse_window_event(body: &[u8]) -> Option<SwayEvent> {
    let event: RawWindowEvent = serde_json::from_slice(body)
        .map_err(|error| tracing::debug!(%error, "unparseable window event"))
        .ok()?;
    let change = event.change?;
    if !INTERESTING_CHANGES.contains(&change.as_str()) {
        return None;
    }
    let container = event.container?;
    if !container.is_window() {
        return None;
    }
    Some(SwayEvent::Window {
        change,
        window: container.to_window(None),
    })
}

#[async_trait]
impl SwayClient for IpcClient {
    async fn get_outputs(&self) -> SwayResult<Vec<Output>> {
        let body = self.request(message::GET_OUTPUTS, b"").await?;
        let raw: Vec<RawOutput> = serde_json::from_slice(&body)?;
        Ok(raw.into_iter().map(Output::from).collect())
    }

    async fn get_windows(&self) -> SwayResult<Vec<Window>> {
        let body = self.request(message::GET_TREE, b"").await?;
        let root: RawNode = serde_json::from_slice(&body)?;
        Ok(collect_windows(&root))
    }

    async fn run_command(&self, command: &str) -> SwayResult<()> {
        let body = self
            .request(message::RUN_COMMAND, command.as_bytes())
            .await?;
        let outcomes: Vec<RawCommandOutcome> = serde_json::from_slice(&body)?;
        if outcomes.is_empty() {
            return Err(SwayError::CommandFailed {
                command: command.to_string(),
                error: "sway returned no result".into(),
            });
        }
        for outcome in outcomes {
            if !outcome.success {
                return Err(SwayError::CommandFailed {
                    command: command.to_string(),
                    error: outcome.error.unwrap_or_else(|| "unknown error".into()),
                });
            }
        }
        Ok(())
    }

    async fn get_version(&self) -> SwayResult<SwayVersion> {
        if let Some(cached) = self.version.read().unwrap().clone() {
            return Ok(cached);
        }
        let body = self.request(message::GET_VERSION, b"").await?;
        let raw: RawVersion = serde_json::from_slice(&body)?;
        let version = SwayVersion {
            major: raw.major,
            minor: raw.minor,
            patch: raw.patch,
            human_readable: raw.human_readable,
        };
        *self.version.write().unwrap() = Some(version.clone());
        Ok(version)
    }

    fn subscribe(&self) -> broadcast::Receiver<SwayEvent> {
        self.events.subscribe()
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_uninteresting_window_changes() {
        let body = br#"{"change":"urgent","container":{"id":1,"type":"con","app_id":"x"}}"#;
        assert!(parse_window_event(body).is_none());
    }

    #[test]
    fn parses_an_interesting_window_change() {
        let body = include_bytes!("fixtures/event_window_new.json");
        let event = parse_window_event(body).expect("event should parse");
        match event {
            SwayEvent::Window { change, window } => {
                assert_eq!(change, "new");
                assert_eq!(window.pid, Some(4242));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn ignores_events_without_a_container() {
        let body = br#"{"change":"new"}"#;
        assert!(parse_window_event(body).is_none());
    }
}
