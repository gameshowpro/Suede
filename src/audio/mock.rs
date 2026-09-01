//! In-memory audio monitor for tests and `--mock`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::{AudioMonitor, AudioResult, NULL_SINK_NAME};
use crate::model::AudioSink;

pub struct MockAudio {
    sinks: RwLock<Vec<AudioSink>>,
    null_sink_created: Mutex<u32>,
    /// Every gain that was asked for, in order, so a test can assert on what
    /// the reconciler did rather than on what it left behind.
    gains_set: Mutex<Vec<(String, f64)>>,
    available: AtomicBool,
    changes: broadcast::Sender<Vec<AudioSink>>,
}

impl Default for MockAudio {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl MockAudio {
    pub fn new(sinks: Vec<AudioSink>) -> Self {
        let (changes, _) = broadcast::channel(16);
        Self {
            sinks: RwLock::new(sinks),
            null_sink_created: Mutex::new(0),
            gains_set: Mutex::new(Vec::new()),
            available: AtomicBool::new(true),
            changes,
        }
    }

    /// Two HDMI sinks, as a small appliance would present.
    pub fn with_sinks() -> Self {
        Self::new(vec![
            AudioSink {
                id: "alsa_output.hdmi-stereo".into(),
                description: Some("HDMI 1".into()),
                is_null_sink: false,
                is_default: true,
                output_hint: None,
                gain_db: Some(0.0),
            },
            AudioSink {
                id: "alsa_output.hdmi-stereo-extra1".into(),
                description: Some("HDMI 2".into()),
                is_null_sink: false,
                is_default: false,
                output_hint: None,
                gain_db: Some(0.0),
            },
        ])
    }

    /// Gains requested so far, in order.
    pub fn gains_set(&self) -> Vec<(String, f64)> {
        self.gains_set.lock().unwrap().clone()
    }

    pub fn set_sinks(&self, sinks: Vec<AudioSink>) {
        *self.sinks.write().unwrap() = sinks.clone();
        let _ = self.changes.send(sinks);
    }

    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Relaxed);
    }

    /// How many times the null sink was created, to assert idempotence.
    pub fn null_sink_creations(&self) -> u32 {
        *self.null_sink_created.lock().unwrap()
    }
}

#[async_trait]
impl AudioMonitor for MockAudio {
    fn sinks(&self) -> Vec<AudioSink> {
        self.sinks.read().unwrap().clone()
    }

    async fn refresh(&self) -> AudioResult<Vec<AudioSink>> {
        Ok(self.sinks())
    }

    async fn set_sink_gain(&self, sink: &str, gain_db: f64) -> AudioResult<()> {
        if !self.sinks().iter().any(|s| s.id == sink) {
            return Err(super::AudioError::SinkUnknown {
                sink: sink.to_string(),
            });
        }
        self.gains_set
            .lock()
            .unwrap()
            .push((sink.to_string(), gain_db));
        let mut sinks = self.sinks();
        for entry in &mut sinks {
            if entry.id == sink {
                entry.gain_db = Some(gain_db);
            }
        }
        self.set_sinks(sinks);
        Ok(())
    }

    async fn ensure_null_sink(&self) -> AudioResult<()> {
        if self.sinks().iter().any(|sink| sink.id == NULL_SINK_NAME) {
            return Ok(());
        }
        *self.null_sink_created.lock().unwrap() += 1;
        let mut sinks = self.sinks();
        sinks.push(AudioSink {
            id: NULL_SINK_NAME.into(),
            description: Some("Suede silent sink".into()),
            is_null_sink: true,
            is_default: false,
            output_hint: None,
            gain_db: Some(0.0),
        });
        self.set_sinks(sinks);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<Vec<AudioSink>> {
        self.changes.subscribe()
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_sink_creation_is_idempotent() {
        let audio = MockAudio::with_sinks();
        audio.ensure_null_sink().await.unwrap();
        audio.ensure_null_sink().await.unwrap();
        assert_eq!(audio.null_sink_creations(), 1);
        assert!(audio.sinks().iter().any(|s| s.is_null_sink));
    }

    #[tokio::test]
    async fn changes_are_broadcast() {
        let audio = MockAudio::default();
        let mut receiver = audio.subscribe();
        audio.set_sinks(vec![AudioSink {
            id: "x".into(),
            description: None,
            is_null_sink: false,
            is_default: false,
            output_hint: None,
            gain_db: Some(0.0),
        }]);
        assert_eq!(receiver.recv().await.unwrap().len(), 1);
    }
}
