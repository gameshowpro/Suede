//! In-memory audio monitor for tests and `--mock`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::{AudioMonitor, AudioResult, NULL_SINK_NAME};
use crate::model::{AudioSink, AudioSource, AvDevices, VideoSource};

pub struct MockAudio {
    devices: RwLock<AvDevices>,
    null_sink_created: Mutex<u32>,
    /// Every gain that was asked for, in order, so a test can assert on what
    /// the reconciler did rather than on what it left behind.
    gains_set: Mutex<Vec<(String, f64)>>,
    available: AtomicBool,
    changes: broadcast::Sender<AvDevices>,
}

impl Default for MockAudio {
    fn default() -> Self {
        Self::new(AvDevices::default())
    }
}

impl MockAudio {
    pub fn new(devices: AvDevices) -> Self {
        let (changes, _) = broadcast::channel(16);
        Self {
            devices: RwLock::new(devices),
            null_sink_created: Mutex::new(0),
            gains_set: Mutex::new(Vec::new()),
            available: AtomicBool::new(true),
            changes,
        }
    }

    /// Two HDMI sinks, one audio input and one video input, modeled on a
    /// Magewell USB capture device, so `--mock` and the smoke test exercise
    /// all three lists.
    pub fn with_devices() -> Self {
        Self::new(AvDevices {
            audio_outputs: vec![
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
            ],
            audio_inputs: vec![AudioSource {
                id: "alsa_input.usb-Magewell_USB_Capture_HDMI_D206190704725-02.analog-stereo"
                    .into(),
                description: Some("USB Capture HDMI Analog Stereo".into()),
                card: Some("USB Capture HDMI".into()),
            }],
            video_inputs: vec![VideoSource {
                id: "v4l2_input.pci-0000_00_14.0-usb-0_6_1.0".into(),
                description: Some("USB Capture HDMI (V4L2)".into()),
                path: Some("/dev/video0".into()),
                card: Some("USB Capture HDMI: USB Capture H".into()),
            }],
        })
    }

    /// Gains requested so far, in order.
    pub fn gains_set(&self) -> Vec<(String, f64)> {
        self.gains_set.lock().unwrap().clone()
    }

    pub fn set_sinks(&self, sinks: Vec<AudioSink>) {
        let mut devices = self.devices();
        devices.audio_outputs = sinks;
        self.set_devices(devices);
    }

    pub fn set_devices(&self, devices: AvDevices) {
        *self.devices.write().unwrap() = devices.clone();
        let _ = self.changes.send(devices);
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
    fn devices(&self) -> AvDevices {
        self.devices.read().unwrap().clone()
    }

    async fn refresh(&self) -> AudioResult<AvDevices> {
        Ok(self.devices())
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

    fn subscribe(&self) -> broadcast::Receiver<AvDevices> {
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
        let audio = MockAudio::with_devices();
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
        assert_eq!(receiver.recv().await.unwrap().audio_outputs.len(), 1);
    }
}
