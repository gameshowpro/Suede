//! The Server-Sent Events hub.
//!
//! Events are state-based, not replayed: a reconnecting client re-fetches
//! current state and then applies events, so `Last-Event-ID` is not supported.

use tokio::sync::broadcast;

use crate::model::{
    AppStatus, AudioSink, Check, ConfigChange, Output, ProjectionStats, Status, WindowChange,
};

const CAPACITY: usize = 256;

/// One named SSE event.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    OutputsChanged(Vec<Output>),
    WindowsChanged(Box<WindowChange>),
    AudioOutputsChanged(Vec<AudioSink>),
    AppStatusChanged(Box<AppStatus>),
    ConfigChanged(ConfigChange),
    StatusChanged(Box<Status>),
    ChecksChanged(Vec<Check>),
    /// Same shape as `GET /projection/stats`; `None` when the slicer stops
    /// or none is running.
    ProjectionStatsChanged(Option<Box<ProjectionStats>>),
}

impl ServerEvent {
    /// The SSE `event:` name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::OutputsChanged(_) => "outputs_changed",
            Self::WindowsChanged(_) => "windows_changed",
            Self::AudioOutputsChanged(_) => "audio_outputs_changed",
            Self::AppStatusChanged(_) => "app_status_changed",
            Self::ConfigChanged(_) => "config_changed",
            Self::StatusChanged(_) => "status_changed",
            Self::ChecksChanged(_) => "checks_changed",
            Self::ProjectionStatsChanged(_) => "projection_stats_changed",
        }
    }

    /// The SSE `data:` payload.
    pub fn data(&self) -> serde_json::Value {
        match self {
            Self::OutputsChanged(outputs) => serde_json::to_value(outputs),
            Self::WindowsChanged(change) => serde_json::to_value(change),
            Self::AudioOutputsChanged(sinks) => serde_json::to_value(sinks),
            Self::AppStatusChanged(status) => serde_json::to_value(status),
            Self::ConfigChanged(change) => serde_json::to_value(change),
            Self::StatusChanged(status) => serde_json::to_value(status),
            Self::ChecksChanged(checks) => serde_json::to_value(checks),
            // `None` must serialise as JSON `null`, not be dropped, so a
            // client sees the slicer stop rather than seeing nothing.
            Self::ProjectionStatsChanged(stats) => serde_json::to_value(stats),
        }
        .unwrap_or(serde_json::Value::Null)
    }
}

/// Fan-out of server events to every connected SSE client.
#[derive(Clone)]
pub struct EventHub {
    sender: broadcast::Sender<ServerEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(CAPACITY);
        Self { sender }
    }

    /// Publish an event. Succeeds silently when nobody is listening.
    pub fn publish(&self, event: ServerEvent) {
        tracing::trace!(event = event.name(), "publishing event");
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.sender.subscribe()
    }

    /// Number of connected SSE clients.
    pub fn listener_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SyncState;

    #[tokio::test]
    async fn delivers_to_subscribers() {
        let hub = EventHub::new();
        let mut receiver = hub.subscribe();
        hub.publish(ServerEvent::OutputsChanged(Vec::new()));
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.name(), "outputs_changed");
        assert_eq!(event.data(), serde_json::json!([]));
    }

    #[tokio::test]
    async fn publishing_without_listeners_is_harmless() {
        let hub = EventHub::new();
        hub.publish(ServerEvent::OutputsChanged(Vec::new()));
        assert_eq!(hub.listener_count(), 0);
    }

    #[test]
    fn status_event_serializes_its_payload() {
        let event = ServerEvent::StatusChanged(Box::new(Status {
            state: SyncState::Degraded,
            ..Default::default()
        }));
        assert_eq!(event.name(), "status_changed");
        assert_eq!(event.data()["state"], "degraded");
    }

    #[test]
    fn projection_stats_stopping_serialises_as_null() {
        // The event a client sees when the slicer stops: not omitted, not
        // absent, but an explicit `null` it can use to blank its display.
        let event = ServerEvent::ProjectionStatsChanged(None);
        assert_eq!(event.name(), "projection_stats_changed");
        assert_eq!(event.data(), serde_json::Value::Null);
    }
}
