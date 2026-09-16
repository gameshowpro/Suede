//! PipeWire monitor backed by `pw-dump` and `pw-cli`.
//!
//! `pw-dump --monitor` is used purely as a change *trigger* — the same way
//! Sway's detail-free `output` event is — and a one-shot `pw-dump` then
//! provides the authoritative list. Change events are only published when the
//! resulting sink list actually differs, so a busy graph cannot cause churn.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, watch};

use super::{AudioError, AudioMonitor, AudioResult, NULL_SINK_NAME};
use crate::model::{AudioSink, AudioSource, AvDevices, VideoSource};

const DUMP: &str = "pw-dump";
const CLI: &str = "pw-cli";
/// WirePlumber's control tool. Preferred for setting a level, because the
/// session manager keeps its own copy of every sink's volume: write the node
/// parameter behind its back and the audio does move, but `wpctl` and every
/// mixer built on it go on reporting the old figure — and may restore it.
const WPCTL: &str = "wpctl";
/// Coalescing window for monitor activity, which can be chatty while audio plays.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(750);

pub struct PipeWireMonitor {
    devices: RwLock<AvDevices>,
    /// Object id and channel count per sink, which setting a level needs and
    /// the public sink list has no business carrying.
    nodes: RwLock<HashMap<String, NodeFacts>>,
    available: AtomicBool,
    changes: broadcast::Sender<AvDevices>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NodeFacts {
    id: u32,
    channels: usize,
}

impl Default for PipeWireMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl PipeWireMonitor {
    pub fn new() -> Self {
        let (changes, _) = broadcast::channel(16);
        Self {
            devices: RwLock::new(AvDevices::default()),
            nodes: RwLock::new(HashMap::new()),
            available: AtomicBool::new(false),
            changes,
        }
    }

    /// Watch PipeWire for changes until shutdown, refreshing the cache as they arrive.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        if let Err(error) = self.refresh().await {
            tracing::warn!(%error, "initial PipeWire query failed; audio features degraded");
        }

        let mut delay = std::time::Duration::from_secs(1);
        loop {
            if *shutdown.borrow() {
                return;
            }
            // The session watches for shutdown on its own clone of the channel.
            let mut session_shutdown = shutdown.clone();
            tokio::select! {
                _ = shutdown.changed() => return,
                result = self.monitor_session(&mut session_shutdown) => {
                    match result {
                        Ok(()) => return,
                        Err(error) => {
                            tracing::warn!(%error, "PipeWire monitor stopped; retrying");
                            self.available.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(std::time::Duration::from_secs(30));
        }
    }

    async fn monitor_session(&self, shutdown: &mut watch::Receiver<bool>) -> AudioResult<()> {
        let mut child = tokio::process::Command::new(DUMP)
            .arg("--monitor")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => AudioError::ToolMissing { tool: DUMP },
                _ => AudioError::ToolFailed {
                    tool: DUMP,
                    detail: error.to_string(),
                },
            })?;

        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut buffer = vec![0u8; 8192];

        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                read = stdout.read(&mut buffer) => {
                    match read {
                        Ok(0) => {
                            return Err(AudioError::ToolFailed {
                                tool: DUMP,
                                detail: "monitor exited".into(),
                            })
                        }
                        Ok(_) => {}
                        Err(error) => {
                            return Err(AudioError::ToolFailed {
                                tool: DUMP,
                                detail: error.to_string(),
                            })
                        }
                    }
                }
            }

            // Drain whatever else arrives inside the debounce window, then
            // re-query once for an authoritative view.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(DEBOUNCE) => break,
                    read = stdout.read(&mut buffer) => {
                        if matches!(read, Ok(0) | Err(_)) {
                            break;
                        }
                    }
                }
            }

            if let Err(error) = self.refresh().await {
                tracing::debug!(%error, "refresh after PipeWire change failed");
            }
        }
    }
}

#[async_trait]
impl AudioMonitor for PipeWireMonitor {
    fn devices(&self) -> AvDevices {
        self.devices.read().unwrap().clone()
    }

    async fn refresh(&self) -> AudioResult<AvDevices> {
        let output =
            tokio::process::Command::new(DUMP)
                .output()
                .await
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => AudioError::ToolMissing { tool: DUMP },
                    _ => AudioError::ToolFailed {
                        tool: DUMP,
                        detail: error.to_string(),
                    },
                })?;

        if !output.status.success() {
            return Err(AudioError::ToolFailed {
                tool: DUMP,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let (devices, nodes) = parse_dump(&output.stdout)?;
        *self.nodes.write().unwrap() = nodes;
        self.available.store(true, Ordering::Relaxed);

        let changed = {
            let mut guard = self.devices.write().unwrap();
            let changed = *guard != devices;
            if changed {
                *guard = devices.clone();
            }
            changed
        };

        if changed {
            tracing::info!(
                audio_outputs = devices.audio_outputs.len(),
                audio_inputs = devices.audio_inputs.len(),
                video_inputs = devices.video_inputs.len(),
                "av devices changed"
            );
            let _ = self.changes.send(devices.clone());
        }
        Ok(devices)
    }

    async fn ensure_null_sink(&self) -> AudioResult<()> {
        if self.sinks().iter().any(|sink| sink.id == NULL_SINK_NAME) {
            return Ok(());
        }

        let properties = format!(
            "{{ factory.name=support.null-audio-sink node.name={NULL_SINK_NAME} \
             node.description=\"Suede silent sink\" media.class=Audio/Sink \
             object.linger=true audio.position=[FL,FR] }}"
        );

        let output = tokio::process::Command::new(CLI)
            .arg("create-node")
            .arg("adapter")
            .arg(&properties)
            .output()
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => AudioError::ToolMissing { tool: CLI },
                _ => AudioError::ToolFailed {
                    tool: CLI,
                    detail: error.to_string(),
                },
            })?;

        if !output.status.success() {
            return Err(AudioError::ToolFailed {
                tool: CLI,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        tracing::info!(sink = NULL_SINK_NAME, "created null audio sink");
        let _ = self.refresh().await;
        Ok(())
    }

    async fn set_sink_gain(&self, sink: &str, gain_db: f64) -> AudioResult<()> {
        let facts = self
            .nodes
            .read()
            .unwrap()
            .get(sink)
            .copied()
            .ok_or_else(|| AudioError::SinkUnknown {
                sink: sink.to_string(),
            })?;
        let linear = super::linear_from_db(gain_db);
        let id = facts.id.to_string();

        // wpctl's number is the cube root of the amplitude, which is why a
        // mixer showing 0.40 is 24 dB down rather than 8.
        let cubic = format!("{:.6}", linear.cbrt());
        match run(WPCTL, &["set-volume", &id, &cubic]).await {
            Ok(()) => {
                tracing::info!(sink, gain_db, "set sink gain");
                let _ = self.refresh().await;
                return Ok(());
            }
            // No WirePlumber on this machine: fall through to the node itself.
            Err(AudioError::ToolMissing { .. }) => {}
            Err(error) => return Err(error),
        }

        let volumes = std::iter::repeat_n(format!("{linear:.6}"), facts.channels.max(1))
            .collect::<Vec<_>>()
            .join(", ");
        run(
            CLI,
            &[
                "set-param",
                &id,
                "Props",
                &format!("{{ channelVolumes: [ {volumes} ] }}"),
            ],
        )
        .await?;
        tracing::info!(sink, gain_db, "set sink gain via the node parameter");
        let _ = self.refresh().await;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<AvDevices> {
        self.changes.subscribe()
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}

/// Run one of the PipeWire tools, mapping its absence and its failures onto
/// [`AudioError`] so a caller can tell "not installed" from "did not work".
async fn run(tool: &'static str, args: &[&str]) -> AudioResult<()> {
    let output = tokio::process::Command::new(tool)
        .args(args)
        .output()
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => AudioError::ToolMissing { tool },
            _ => AudioError::ToolFailed {
                tool,
                detail: error.to_string(),
            },
        })?;
    if !output.status.success() {
        return Err(AudioError::ToolFailed {
            tool,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

// --- pw-dump JSON shapes -------------------------------------------------

#[derive(Debug, Deserialize)]
struct PwObject {
    id: Option<u32>,
    #[serde(rename = "type")]
    object_type: Option<String>,
    info: Option<PwInfo>,
    #[serde(default)]
    metadata: Vec<PwMetadata>,
}

#[derive(Debug, Deserialize)]
struct PwInfo {
    props: Option<HashMap<String, serde_json::Value>>,
    params: Option<PwParams>,
}

#[derive(Debug, Deserialize)]
struct PwParams {
    /// The mixer parameters. PipeWire reports a list; the volumes live in
    /// whichever entry carries them, so take the first that does.
    #[serde(rename = "Props", default)]
    props: Vec<serde_json::Value>,
}

/// Per-channel linear amplitudes, if this node reports any.
fn channel_volumes(info: &PwInfo) -> Option<Vec<f64>> {
    let params = info.params.as_ref()?;
    params.props.iter().find_map(|entry| {
        let values = entry.get("channelVolumes")?.as_array()?;
        let volumes: Vec<f64> = values
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .collect();
        (!volumes.is_empty()).then_some(volumes)
    })
}

#[derive(Debug, Deserialize)]
struct PwMetadata {
    key: Option<String>,
    value: Option<serde_json::Value>,
}

fn property<'a>(props: &'a HashMap<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(serde_json::Value::as_str)
}

/// Extract every audio and video device, and which sink PipeWire currently
/// treats as default.
pub fn parse_devices(dump: &[u8]) -> AudioResult<AvDevices> {
    parse_dump(dump).map(|(devices, _)| devices)
}

/// The same walk, also returning what setting a level needs: each sink's
/// object id and how many channels it carries.
fn parse_dump(dump: &[u8]) -> AudioResult<(AvDevices, HashMap<String, NodeFacts>)> {
    let objects: Vec<PwObject> =
        serde_json::from_slice(dump).map_err(|source| AudioError::Parse { tool: DUMP, source })?;

    let mut default_sink: Option<String> = None;
    for object in &objects {
        for entry in &object.metadata {
            if entry.key.as_deref() == Some("default.audio.sink") {
                default_sink = entry
                    .value
                    .as_ref()
                    .and_then(|value| {
                        value
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .or_else(|| value.as_str())
                    })
                    .map(str::to_string);
            }
        }
    }

    let mut devices = AvDevices::default();
    let mut nodes: HashMap<String, NodeFacts> = HashMap::new();
    for object in &objects {
        if object.object_type.as_deref() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(props) = object.info.as_ref().and_then(|info| info.props.as_ref()) else {
            continue;
        };
        let Some(name) = property(props, "node.name") else {
            continue;
        };

        match property(props, "media.class") {
            Some("Audio/Sink") => {
                let info = object.info.as_ref().expect("props came from info");
                let volumes = channel_volumes(info);
                if let Some(id) = object.id {
                    nodes.insert(
                        name.to_string(),
                        NodeFacts {
                            id,
                            channels: volumes.as_ref().map_or(2, Vec::len),
                        },
                    );
                }

                devices.audio_outputs.push(AudioSink {
                    id: name.to_string(),
                    description: property(props, "node.description").map(str::to_string),
                    // Any sink built on the null factory discards what it is
                    // given: the one Suede manages for silent routing, and the
                    // `auto_null` dummy PipeWire falls back to when it can see
                    // no audio devices at all.
                    is_null_sink: name == NULL_SINK_NAME
                        || property(props, "factory.name") == Some("support.null-audio-sink"),
                    is_default: default_sink.as_deref() == Some(name),
                    output_hint: property(props, "api.alsa.path")
                        .or_else(|| property(props, "api.alsa.pcm.name"))
                        .map(str::to_string),
                    // One figure for a sink whose channels could in principle
                    // differ: the loudest, because that is the one that will
                    // clip.
                    gain_db: volumes.as_ref().map(|v| {
                        super::round_db(super::db_from_linear(
                            v.iter().copied().fold(0.0_f64, f64::max),
                        ))
                    }),
                });
            }
            Some("Audio/Source") => {
                devices.audio_inputs.push(AudioSource {
                    id: name.to_string(),
                    description: property(props, "node.description").map(str::to_string),
                    card: property(props, "api.alsa.card.name").map(str::to_string),
                });
            }
            Some("Video/Source") => {
                devices.video_inputs.push(VideoSource {
                    id: name.to_string(),
                    description: property(props, "node.description").map(str::to_string),
                    path: property(props, "api.v4l2.path").map(str::to_string),
                    card: property(props, "api.v4l2.cap.card").map(str::to_string),
                });
            }
            _ => {}
        }
    }

    // Stable ordering keeps change detection meaningful and the API predictable.
    devices.audio_outputs.sort_by(|a, b| a.id.cmp(&b.id));
    devices.audio_inputs.sort_by(|a, b| a.id.cmp(&b.id));
    devices.video_inputs.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((devices, nodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP_FIXTURE: &[u8] = include_bytes!("fixtures/pw_dump.json");

    #[test]
    fn parses_sinks_from_a_real_dump() {
        let sinks = parse_devices(DUMP_FIXTURE).unwrap().audio_outputs;
        assert_eq!(sinks.len(), 4);
        let ids: Vec<&str> = sinks.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"alsa_output.pci-0000_01_00.1.hdmi-stereo"));
        assert!(ids.contains(&NULL_SINK_NAME));
    }

    #[test]
    fn ignores_sources_and_non_node_objects() {
        let sinks = parse_devices(DUMP_FIXTURE).unwrap().audio_outputs;
        assert!(sinks.iter().all(|sink| !sink.id.contains("input")));
    }

    #[test]
    fn identifies_the_default_sink() {
        let sinks = parse_devices(DUMP_FIXTURE).unwrap().audio_outputs;
        let default: Vec<&AudioSink> = sinks.iter().filter(|s| s.is_default).collect();
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].id, "alsa_output.pci-0000_01_00.1.hdmi-stereo");
    }

    #[test]
    fn flags_the_suede_null_sink() {
        let sinks = parse_devices(DUMP_FIXTURE).unwrap().audio_outputs;
        let null = sinks.iter().find(|s| s.id == NULL_SINK_NAME).unwrap();
        assert!(null.is_null_sink);
        assert!(!null.is_default);
    }

    #[test]
    fn pipewires_own_fallback_dummy_counts_as_a_null_sink() {
        // When PipeWire can open no devices it invents `auto_null`. Reading
        // that as a working output is how a machine with no audio at all
        // reports itself healthy.
        let sinks = parse_devices(DUMP_FIXTURE).unwrap().audio_outputs;
        let dummy = sinks.iter().find(|s| s.id == "auto_null").unwrap();
        assert!(dummy.is_null_sink);
        // Real devices are still told apart from both dummies.
        assert_eq!(sinks.iter().filter(|s| !s.is_null_sink).count(), 2);
    }

    #[test]
    fn carries_descriptions_and_hints() {
        let sinks = parse_devices(DUMP_FIXTURE).unwrap().audio_outputs;
        let hdmi = sinks
            .iter()
            .find(|s| s.id == "alsa_output.pci-0000_01_00.1.hdmi-stereo")
            .unwrap();
        assert_eq!(hdmi.description.as_deref(), Some("Acme HDMI / DisplayPort"));
        assert_eq!(hdmi.output_hint.as_deref(), Some("hdmi:CARD=HDMI,DEV=0"));
    }

    #[test]
    fn parses_audio_inputs() {
        let inputs = parse_devices(DUMP_FIXTURE).unwrap().audio_inputs;
        assert_eq!(inputs.len(), 1);
        let built_in = &inputs[0];
        assert_eq!(built_in.id, "alsa_input.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(
            built_in.description.as_deref(),
            Some("Built-in Audio Analog Stereo")
        );
        assert_eq!(built_in.card.as_deref(), Some("Built-in Audio"));
    }

    #[test]
    fn parses_video_inputs() {
        let inputs = parse_devices(DUMP_FIXTURE).unwrap().video_inputs;
        assert_eq!(inputs.len(), 1);
        let capture = &inputs[0];
        assert_eq!(capture.id, "v4l2_input.pci-0000_00_14.0-usb-0_6_1.0");
        assert_eq!(
            capture.description.as_deref(),
            Some("USB Capture HDMI (V4L2)")
        );
        assert_eq!(capture.path.as_deref(), Some("/dev/video0"));
        assert_eq!(
            capture.card.as_deref(),
            Some("USB Capture HDMI: USB Capture H")
        );
    }

    #[test]
    fn only_outputs_carry_a_default() {
        // AudioSource and VideoSource have no `is_default` field at all;
        // assert that by serialising and checking the key is absent, rather
        // than by a field access that the compiler would just refuse.
        let devices = parse_devices(DUMP_FIXTURE).unwrap();
        let json = serde_json::to_value(&devices).unwrap();
        let input = &json["audioInputs"][0];
        assert!(input.get("isDefault").is_none());
    }

    #[test]
    fn all_device_lists_are_sorted_for_stable_change_detection() {
        let devices = parse_devices(DUMP_FIXTURE).unwrap();

        let mut sorted_outputs = devices.audio_outputs.clone();
        sorted_outputs.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(devices.audio_outputs, sorted_outputs);

        let mut sorted_inputs = devices.audio_inputs.clone();
        sorted_inputs.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(devices.audio_inputs, sorted_inputs);

        let mut sorted_video = devices.video_inputs.clone();
        sorted_video.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(devices.video_inputs, sorted_video);
    }

    #[test]
    fn empty_dump_yields_no_devices() {
        let devices = parse_devices(b"[]").unwrap();
        assert!(devices.audio_outputs.is_empty());
        assert!(devices.audio_inputs.is_empty());
        assert!(devices.video_inputs.is_empty());
    }

    #[test]
    fn malformed_dump_is_an_error_not_a_panic() {
        assert!(parse_devices(b"not json").is_err());
    }
}
