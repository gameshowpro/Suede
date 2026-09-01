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
    #[error("no audio sink is named {sink}")]
    SinkUnknown { sink: String },
}

/// Bottom of the fader, in dB, and the one value that is not a gain at all:
/// it means silence.
///
/// Every level Suede states is in dB, so the scale needs a bottom — the true
/// zero of a linear amplitude is minus infinity, which no configuration file
/// can hold and no operator wants to type. -100 dB stands in for it and is
/// applied as an exact zero, not as a very small number.
pub const GAIN_FLOOR_DB: f64 = -100.0;

/// Loudest gain Suede will set. Unity is the ceiling: above it a digital sink
/// has no headroom left and simply clips. Gain belongs in the amplifier.
pub const GAIN_CEILING_DB: f64 = 0.0;

/// Linear amplitude for a gain in dB — PipeWire's own scale for
/// `channelVolumes`, and not the one a mixer UI displays.
pub fn linear_from_db(db: f64) -> f64 {
    if db <= GAIN_FLOOR_DB {
        0.0
    } else {
        10f64.powf(db / 20.0)
    }
}

/// Quantise a level for reporting and comparison.
///
/// A tenth of a decibel is already an order of magnitude finer than anyone can
/// hear, and it is what a mixer shows. Reporting the raw conversion instead
/// gives readings like `-23.876400520322257`, which invites the reader to
/// believe in a precision the whole signal path does not have.
///
/// It also settles comparisons: a level that has been through amplitude and
/// back is never bit-identical to the one that was asked for, so quantising
/// both sides is what stops the reconciler rewriting an unchanged value on
/// every pass.
pub fn round_db(db: f64) -> f64 {
    (db * 10.0).round() / 10.0
}

/// The inverse, with the floor standing in for digital silence.
pub fn db_from_linear(linear: f64) -> f64 {
    if linear <= 0.0 {
        GAIN_FLOOR_DB
    } else {
        (20.0 * linear.log10()).max(GAIN_FLOOR_DB)
    }
}

#[cfg(test)]
mod gain_tests {
    use super::*;

    #[test]
    fn unity_is_one_and_the_floor_is_silence() {
        assert!((linear_from_db(0.0) - 1.0).abs() < 1e-12);
        assert_eq!(linear_from_db(GAIN_FLOOR_DB), 0.0);
        assert_eq!(linear_from_db(-120.0), 0.0);
        assert_eq!(db_from_linear(0.0), GAIN_FLOOR_DB);
    }

    #[test]
    fn halving_the_amplitude_is_six_decibels() {
        assert!((db_from_linear(0.5) + 6.0206).abs() < 1e-3);
        assert!((linear_from_db(-6.0206) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn reported_levels_are_quantised_to_a_tenth() {
        assert_eq!(round_db(-23.876400520322257), -23.9);
        assert_eq!(round_db(0.0), 0.0);
        assert_eq!(round_db(-18.04), -18.0);
        // The point of it: a level that has been through amplitude and back
        // compares equal to the one that was asked for.
        assert_eq!(round_db(db_from_linear(linear_from_db(-6.0))), -6.0);
    }

    #[test]
    fn a_round_trip_returns_what_it_was_given() {
        for db in [-96.0, -48.0, -24.0, -18.0, -6.0, 0.0] {
            assert!(
                (db_from_linear(linear_from_db(db)) - db).abs() < 1e-9,
                "{db}"
            );
        }
    }
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

    /// Set a sink's playback gain, in dB, `0.0` being unity.
    async fn set_sink_gain(&self, sink: &str, gain_db: f64) -> AudioResult<()>;

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
        let config = AudioConfig {
            output: None,
            gain_db: 0.0,
        };
        assert_eq!(
            resolve_pulse_sink(Some(&config)).as_deref(),
            Some(NULL_SINK_NAME)
        );
    }

    #[test]
    fn named_output_is_used_verbatim() {
        let config = AudioConfig {
            output: Some("alsa_output.hdmi-stereo".into()),
            gain_db: 0.0,
        };
        assert_eq!(
            resolve_pulse_sink(Some(&config)).as_deref(),
            Some("alsa_output.hdmi-stereo")
        );
    }
}
