//! Keeps one blend-overlay process running per output that has seams.
//!
//! Deliberately simpler than the app supervisor: overlays are stateless and
//! instant to start, so there is no backoff, no window tracking, and no
//! restart policy — just "the set of running overlays matches the set of
//! specs". A dead overlay is respawned on the next reconciliation pass,
//! which is also what bounds the respawn rate.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::process::{Child, Command, Stdio};

use crate::model::Divergence;

use super::blend::OverlaySpec;

struct RunningOverlay {
    child: Child,
    fingerprint: u64,
}

#[derive(Default)]
pub struct BlendManager {
    overlays: HashMap<String, RunningOverlay>,
}

/// What a sync pass decided, before any process is touched. Pure, so the
/// decision rules are testable without spawning anything.
#[derive(Debug, PartialEq, Eq)]
struct SyncPlan {
    kill: Vec<String>,
    spawn: Vec<String>,
}

fn plan_sync(running: &HashMap<String, u64>, wanted: &HashMap<String, u64>) -> SyncPlan {
    let mut kill: Vec<String> = running
        .iter()
        .filter(|(output, fingerprint)| wanted.get(*output) != Some(fingerprint))
        .map(|(output, _)| output.clone())
        .collect();
    let mut spawn: Vec<String> = wanted
        .iter()
        .filter(|(output, fingerprint)| running.get(*output) != Some(fingerprint))
        .map(|(output, _)| output.clone())
        .collect();
    kill.sort();
    spawn.sort();
    SyncPlan { kill, spawn }
}

fn fingerprint(spec: &OverlaySpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(spec)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

impl BlendManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the running overlays match `specs`. Empty specs — blending
    /// disabled, no seams, or no projection section at all — tears every
    /// overlay down and reports nothing.
    pub fn sync(&mut self, specs: &[OverlaySpec]) -> Vec<Divergence> {
        let mut divergences = Vec::new();

        // Reap exits first, so a crashed overlay counts as "not running" and
        // is respawned below rather than trusted.
        self.overlays
            .retain(|output, running| match running.child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    tracing::warn!(output, %status, "blend overlay exited; will respawn");
                    false
                }
                Err(error) => {
                    tracing::warn!(output, %error, "could not check blend overlay");
                    false
                }
            });

        let wanted: HashMap<String, u64> = specs
            .iter()
            .map(|spec| (spec.output.clone(), fingerprint(spec)))
            .collect();
        let running: HashMap<String, u64> = self
            .overlays
            .iter()
            .map(|(output, overlay)| (output.clone(), overlay.fingerprint))
            .collect();
        let plan = plan_sync(&running, &wanted);

        for output in &plan.kill {
            if let Some(mut overlay) = self.overlays.remove(output) {
                tracing::info!(output, "stopping blend overlay");
                let _ = overlay.child.kill();
                let _ = overlay.child.wait();
            }
        }

        for output in &plan.spawn {
            let spec = specs
                .iter()
                .find(|spec| &spec.output == output)
                .expect("planned spawns come from specs");
            match spawn_overlay(spec) {
                Ok(child) => {
                    tracing::info!(
                        output,
                        ramps = spec.ramps.len(),
                        gamma = spec.gamma,
                        "started blend overlay"
                    );
                    self.overlays.insert(
                        output.clone(),
                        RunningOverlay {
                            child,
                            fingerprint: wanted[output],
                        },
                    );
                }
                Err(error) => divergences.push(Divergence::new(
                    "blend_overlay_failed",
                    output,
                    format!("could not start the blend overlay: {error}"),
                )),
            }
        }

        divergences
    }

    /// Kill everything. Called at daemon shutdown so no overlay outlives the
    /// configuration that asked for it.
    pub fn shutdown(&mut self) {
        for (output, mut overlay) in self.overlays.drain() {
            tracing::debug!(output, "stopping blend overlay");
            let _ = overlay.child.kill();
            let _ = overlay.child.wait();
        }
    }
}

impl Drop for BlendManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The overlay is this same binary: one ELF on disk, per the packaging story.
fn spawn_overlay(spec: &OverlaySpec) -> std::io::Result<Child> {
    let program = std::env::current_exe()?;
    Command::new(program)
        .arg("blend")
        .arg("--spec")
        .arg(serde_json::to_string(spec)?)
        .stdin(Stdio::null())
        // Stderr flows to the daemon's own journal, tagged per process.
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, u64)]) -> HashMap<String, u64> {
        entries
            .iter()
            .map(|(name, fingerprint)| (name.to_string(), *fingerprint))
            .collect()
    }

    #[test]
    fn an_empty_want_kills_everything_and_spawns_nothing() {
        let plan = plan_sync(&map(&[("DP-1", 1), ("DP-2", 2)]), &map(&[]));
        assert_eq!(plan.kill, vec!["DP-1", "DP-2"]);
        assert!(plan.spawn.is_empty());
    }

    #[test]
    fn a_matching_state_is_left_alone() {
        let plan = plan_sync(&map(&[("DP-1", 1)]), &map(&[("DP-1", 1)]));
        assert!(plan.kill.is_empty() && plan.spawn.is_empty());
    }

    #[test]
    fn a_changed_spec_is_killed_and_respawned() {
        // The overlay draws a static image, so a new spec means a new process.
        let plan = plan_sync(&map(&[("DP-1", 1)]), &map(&[("DP-1", 2)]));
        assert_eq!(plan.kill, vec!["DP-1"]);
        assert_eq!(plan.spawn, vec!["DP-1"]);
    }

    #[test]
    fn fingerprints_track_the_spec_content() {
        use super::super::blend::{FadeTo, RampSpec};
        use crate::model::Rect;
        let mut spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.0,
            ramps: vec![RampSpec {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 160,
                    height: 1080,
                },
                fade_to: FadeTo::Left,
            }],
        };
        let original = fingerprint(&spec);
        assert_eq!(original, fingerprint(&spec), "stable for equal specs");
        spec.gamma = 2.4;
        assert_ne!(original, fingerprint(&spec), "gamma changes must repaint");
    }
}
