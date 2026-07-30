//! Audio sink enumeration and routing.
//!
//! Audio is outside Sway's scope, so Suede talks to PipeWire directly. It does
//! so through PipeWire's own command-line tools (`pw-dump`, `pw-cli`) rather
//! than the `pipewire` crate: that keeps the binary free of native library
//! dependencies, which is what lets it stay a single self-contained ELF and
//! cross-compile to aarch64 without a PipeWire sysroot.

pub mod mock;
pub mod pw;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::model::AudioSink;

/// Name of the null sink Suede manages for silent routing.
pub const NULL_SINK_NAME: &str = "suede-null";

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("{tool} is not installed or not on PATH")]
    ToolMissing { tool: &'static str },
    #[error("{tool} failed: {detail}")]
    ToolFailed { tool: &'static str, detail: String },
    #[error("failed to parse {tool} output: {source}")]
    Parse {
        tool: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

pub type AudioResult<T> = Result<T, AudioError>;

/// Sink enumeration and change notification.
#[async_trait]
pub trait AudioMonitor: Send + Sync + 'static {
    /// Most recently observed sinks.
    fn sinks(&self) -> Vec<AudioSink>;

    /// Re-query PipeWire and update the cache.
    async fn refresh(&self) -> AudioResult<Vec<AudioSink>>;

    /// Create the null sink if it does not already exist.
    async fn ensure_null_sink(&self) -> AudioResult<()>;

    /// Receiver notified whenever the sink list actually changes.
    fn subscribe(&self) -> broadcast::Receiver<Vec<AudioSink>>;

    /// Whether PipeWire has answered at least once.
    fn is_available(&self) -> bool;
}

/// Resolve an app's configured sink to the value of `PULSE_SINK`.
///
/// Returns `None` when the app should inherit the default routing.
pub fn resolve_pulse_sink(audio: Option<&crate::model::AudioConfig>) -> Option<String> {
    // Absent leaves routing alone; present-but-null routes to silence.
    audio.map(|config| {
        config
            .output
            .clone()
            .unwrap_or_else(|| NULL_SINK_NAME.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AudioConfig;

    #[test]
    fn absent_audio_config_leaves_routing_untouched() {
        assert_eq!(resolve_pulse_sink(None), None);
    }

    #[test]
    fn null_output_routes_to_the_null_sink() {
        let config = AudioConfig { output: None };
        assert_eq!(
            resolve_pulse_sink(Some(&config)).as_deref(),
            Some(NULL_SINK_NAME)
        );
    }

    #[test]
    fn named_output_is_used_verbatim() {
        let config = AudioConfig {
            output: Some("alsa_output.hdmi-stereo".into()),
        };
        assert_eq!(
            resolve_pulse_sink(Some(&config)).as_deref(),
            Some("alsa_output.hdmi-stereo")
        );
    }
}
