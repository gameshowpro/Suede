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
use crate::model::AudioSink;

const DUMP: &str = "pw-dump";
const CLI: &str = "pw-cli";
/// Coalescing window for monitor activity, which can be chatty while audio plays.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(750);

pub struct PipeWireMonitor {
    sinks: RwLock<Vec<AudioSink>>,
    available: AtomicBool,
    changes: broadcast::Sender<Vec<AudioSink>>,
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
            sinks: RwLock::new(Vec::new()),
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
    fn sinks(&self) -> Vec<AudioSink> {
        self.sinks.read().unwrap().clone()
    }

    async fn refresh(&self) -> AudioResult<Vec<AudioSink>> {
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

        let sinks = parse_sinks(&output.stdout)?;
        self.available.store(true, Ordering::Relaxed);

        let changed = {
            let mut guard = self.sinks.write().unwrap();
            let changed = *guard != sinks;
            if changed {
                *guard = sinks.clone();
            }
            changed
        };

        if changed {
            tracing::info!(count = sinks.len(), "audio sinks changed");
            let _ = self.changes.send(sinks.clone());
        }
        Ok(sinks)
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

    fn subscribe(&self) -> broadcast::Receiver<Vec<AudioSink>> {
        self.changes.subscribe()
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}

// --- pw-dump JSON shapes -------------------------------------------------

#[derive(Debug, Deserialize)]
struct PwObject {
    #[serde(rename = "type")]
    object_type: Option<String>,
    info: Option<PwInfo>,
    #[serde(default)]
    metadata: Vec<PwMetadata>,
}

#[derive(Debug, Deserialize)]
struct PwInfo {
    props: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct PwMetadata {
    key: Option<String>,
    value: Option<serde_json::Value>,
}

fn property<'a>(props: &'a HashMap<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(serde_json::Value::as_str)
}

/// Extract audio sinks, and which one PipeWire currently treats as default.
pub fn parse_sinks(dump: &[u8]) -> AudioResult<Vec<AudioSink>> {
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

    let mut sinks = Vec::new();
    for object in &objects {
        if object.object_type.as_deref() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(props) = object.info.as_ref().and_then(|info| info.props.as_ref()) else {
            continue;
        };
        if property(props, "media.class") != Some("Audio/Sink") {
            continue;
        }
        let Some(name) = property(props, "node.name") else {
            continue;
        };

        sinks.push(AudioSink {
            id: name.to_string(),
            description: property(props, "node.description").map(str::to_string),
            // Any sink built on the null factory discards what it is
            // given: the one Suede manages for silent routing, and the
            // `auto_null` dummy PipeWire falls back to when it can see no
            // audio devices at all.
            is_null_sink: name == NULL_SINK_NAME
                || property(props, "factory.name") == Some("support.null-audio-sink"),
            is_default: default_sink.as_deref() == Some(name),
            output_hint: property(props, "api.alsa.path")
                .or_else(|| property(props, "api.alsa.pcm.name"))
                .map(str::to_string),
        });
    }

    // Stable ordering keeps change detection meaningful and the API predictable.
    sinks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(sinks)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP_FIXTURE: &[u8] = include_bytes!("fixtures/pw_dump.json");

    #[test]
    fn parses_sinks_from_a_real_dump() {
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        assert_eq!(sinks.len(), 4);
        let ids: Vec<&str> = sinks.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"alsa_output.pci-0000_01_00.1.hdmi-stereo"));
        assert!(ids.contains(&NULL_SINK_NAME));
    }

    #[test]
    fn ignores_sources_and_non_node_objects() {
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        assert!(sinks.iter().all(|sink| !sink.id.contains("input")));
    }

    #[test]
    fn identifies_the_default_sink() {
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        let default: Vec<&AudioSink> = sinks.iter().filter(|s| s.is_default).collect();
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].id, "alsa_output.pci-0000_01_00.1.hdmi-stereo");
    }

    #[test]
    fn flags_the_suede_null_sink() {
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        let null = sinks.iter().find(|s| s.id == NULL_SINK_NAME).unwrap();
        assert!(null.is_null_sink);
        assert!(!null.is_default);
    }

    #[test]
    fn pipewires_own_fallback_dummy_counts_as_a_null_sink() {
        // When PipeWire can open no devices it invents `auto_null`. Reading
        // that as a working output is how a machine with no audio at all
        // reports itself healthy.
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        let dummy = sinks.iter().find(|s| s.id == "auto_null").unwrap();
        assert!(dummy.is_null_sink);
        // Real devices are still told apart from both dummies.
        assert_eq!(sinks.iter().filter(|s| !s.is_null_sink).count(), 2);
    }

    #[test]
    fn carries_descriptions_and_hints() {
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        let hdmi = sinks
            .iter()
            .find(|s| s.id == "alsa_output.pci-0000_01_00.1.hdmi-stereo")
            .unwrap();
        assert_eq!(hdmi.description.as_deref(), Some("Acme HDMI / DisplayPort"));
        assert_eq!(hdmi.output_hint.as_deref(), Some("hdmi:CARD=HDMI,DEV=0"));
    }

    #[test]
    fn sinks_are_sorted_for_stable_change_detection() {
        let sinks = parse_sinks(DUMP_FIXTURE).unwrap();
        let mut sorted = sinks.clone();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sinks, sorted);
    }

    #[test]
    fn empty_dump_yields_no_sinks() {
        assert!(parse_sinks(b"[]").unwrap().is_empty());
    }

    #[test]
    fn malformed_dump_is_an_error_not_a_panic() {
        assert!(parse_sinks(b"not json").is_err());
    }
}
