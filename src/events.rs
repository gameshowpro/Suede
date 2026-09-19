//! The Server-Sent Events hub.
//!
//! Events are state-based, not replayed: a reconnecting client re-fetches
//! current state and then applies events, so `Last-Event-ID` is not supported.

use tokio::sync::broadcast;

use crate::model::{
    AppStatus, AvDevices, Check, ConfigChange, Output, ProjectionReport, Status, WindowChange,
};

const CAPACITY: usize = 256;

/// One named SSE event.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    OutputsChanged(Vec<Output>),
    WindowsChanged(Box<WindowChange>),
    AvChanged(AvDevices),
    AppStatusChanged(Box<AppStatus>),
    ConfigChanged(ConfigChange),
    StatusChanged(Box<Status>),
    ChecksChanged(Vec<Check>),
    /// Same shape as `GET /projection/stats`: whether a slicer is alive,
    /// plus its last reported interval, if any. Published whenever either
    /// half changes, so a client that only watches events also learns that
    /// a running slicer has simply reported nothing yet, rather than
    /// reading that as "not running" — see `ProjectionReport`.
    ProjectionStatsChanged(Box<ProjectionReport>),
}

impl ServerEvent {
    /// The SSE `event:` name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::OutputsChanged(_) => "outputs_changed",
            Self::WindowsChanged(_) => "windows_changed",
            Self::AvChanged(_) => "av_changed",
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
            Self::AvChanged(devices) => serde_json::to_value(devices),
            Self::AppStatusChanged(status) => serde_json::to_value(status),
            Self::ConfigChanged(change) => serde_json::to_value(change),
            Self::StatusChanged(status) => serde_json::to_value(status),
            Self::ChecksChanged(checks) => serde_json::to_value(checks),
            Self::ProjectionStatsChanged(report) => serde_json::to_value(report),
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
    fn av_changed_event_serializes_the_whole_device_set() {
        let event = ServerEvent::AvChanged(crate::model::AvDevices {
            audio_outputs: vec![crate::model::AudioSink {
                id: "alsa_output.hdmi-stereo".into(),
                description: Some("HDMI".into()),
                is_null_sink: false,
                is_default: true,
                output_hint: None,
                gain_db: Some(0.0),
            }],
            audio_inputs: vec![crate::model::AudioSource {
                id: "alsa_input.usb".into(),
                description: Some("USB Capture HDMI Analog Stereo".into()),
                card: Some("USB Capture HDMI".into()),
            }],
            video_inputs: vec![],
        });
        assert_eq!(event.name(), "av_changed");
        let data = event.data();
        assert_eq!(data["audioOutputs"][0]["id"], "alsa_output.hdmi-stereo");
        assert_eq!(data["audioInputs"][0]["card"], "USB Capture HDMI");
        assert_eq!(data["videoInputs"], serde_json::json!([]));
    }

    #[test]
    fn projection_stats_stopping_reports_not_running() {
        // The event a client sees when the slicer stops: an object saying
        // `running: false`, not a bare `null` a client could equally read
        // as "still running, just quiet" — the ambiguity this type exists
        // to remove.
        let event = ServerEvent::ProjectionStatsChanged(Box::new(crate::model::ProjectionReport {
            geometry: Default::default(),
            running: false,
            last_interval: None,
            control: Default::default(),
        }));
        assert_eq!(event.name(), "projection_stats_changed");
        assert_eq!(event.data(), serde_json::json!({"running": false}));
    }
}
