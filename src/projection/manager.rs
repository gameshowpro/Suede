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
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;

use crate::events::{EventHub, ServerEvent};
use crate::model::Divergence;
use crate::snapshot::Snapshot;

use super::blend::{OverlaySpec, SlicerSpec};

struct RunningOverlay {
    child: Child,
    fingerprint: u64,
}

pub struct BlendManager {
    overlays: HashMap<String, RunningOverlay>,
    /// The one slicer process for the whole installation, in canvas mode.
    slicer: Option<RunningOverlay>,
    /// Where the slicer's stdout JSON lines land, and where its absence is
    /// announced — shared with the reconciler and the API.
    snapshot: Arc<Snapshot>,
    events: EventHub,
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
    pub fn new(snapshot: Arc<Snapshot>, events: EventHub) -> Self {
        Self {
            overlays: HashMap::new(),
            slicer: None,
            snapshot,
            events,
        }
    }

    /// Clear the last-reported projection stats and tell any SSE listener,
    /// but only if there was something to clear — a manager that never ran a
    /// slicer must not spam a `null` on every reconciliation pass.
    fn clear_projection_stats(&self) {
        if self.snapshot.set_projection_stats(None) {
            self.events
                .publish(ServerEvent::ProjectionStatsChanged(None));
        }
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

    /// Make the running slicer match `spec`. `None` tears it down.
    pub fn sync_slicer(&mut self, spec: Option<&SlicerSpec>) -> Vec<Divergence> {
        // Reap an exit first so a crashed slicer is respawned, not trusted.
        if let Some(running) = &mut self.slicer {
            let exited = match running.child.try_wait() {
                Ok(None) => false,
                Ok(Some(status)) => {
                    tracing::warn!(%status, "slicer exited; will respawn");
                    true
                }
                Err(error) => {
                    tracing::warn!(%error, "could not check the slicer");
                    true
                }
            };
            if exited {
                self.slicer = None;
                self.clear_projection_stats();
            }
        }

        let wanted = spec.map(|spec| {
            let mut hasher = DefaultHasher::new();
            serde_json::to_string(spec)
                .unwrap_or_default()
                .hash(&mut hasher);
            hasher.finish()
        });
        let running = self.slicer.as_ref().map(|s| s.fingerprint);
        if wanted == running {
            return Vec::new();
        }

        if let Some(mut old) = self.slicer.take() {
            tracing::info!("stopping the slicer");
            let _ = old.child.kill();
            let _ = old.child.wait();
            self.clear_projection_stats();
        }
        let (Some(spec), Some(fingerprint)) = (spec, wanted) else {
            return Vec::new();
        };
        match spawn_internal(
            "slice",
            &serde_json::to_string(spec).unwrap_or_default(),
            true,
        ) {
            Ok(mut child) => {
                tracing::info!(
                    canvas = format!("{}x{}", spec.canvas_width, spec.canvas_height),
                    slices = spec.slices.len(),
                    "started the slicer"
                );
                // Piped because `pipe_stdout` was true above; `take()` still
                // returns `None` if the platform ever refuses a pipe, in
                // which case the slicer just runs without reported stats.
                if let Some(stdout) = child.stdout.take() {
                    spawn_stats_reader(stdout, self.snapshot.clone(), self.events.clone());
                }
                self.slicer = Some(RunningOverlay { child, fingerprint });
                Vec::new()
            }
            Err(error) => vec![Divergence::new(
                "blend_overlay_failed",
                "slicer",
                format!("could not start the slicer: {error}"),
            )],
        }
    }

    /// Kill everything. Called at daemon shutdown so no overlay outlives the
    /// configuration that asked for it.
    pub fn shutdown(&mut self) {
        for (output, mut overlay) in self.overlays.drain() {
            tracing::debug!(output, "stopping blend overlay");
            let _ = overlay.child.kill();
            let _ = overlay.child.wait();
        }
        if let Some(mut slicer) = self.slicer.take() {
            tracing::debug!("stopping the slicer");
            let _ = slicer.child.kill();
            let _ = slicer.child.wait();
            self.clear_projection_stats();
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
    spawn_internal("blend", &serde_json::to_string(spec)?, false)
}

/// `pipe_stdout` is true only for the slicer: it reports `ProjectionStats` as
/// JSON lines on stdout (see `crate::projection::slicer`), which this
/// process reads back with [`spawn_stats_reader`]. Blend overlays have
/// nothing to say there, so theirs keeps inheriting the daemon's stdout —
/// piping it for no reason would just make it silently vanish.
fn spawn_internal(subcommand: &str, spec: &str, pipe_stdout: bool) -> std::io::Result<Child> {
    let program = std::env::current_exe()?;
    let mut command = Command::new(program);
    command
        .arg(subcommand)
        .arg("--spec")
        .arg(spec)
        .stdin(Stdio::null());
    if pipe_stdout {
        command.stdout(Stdio::piped());
    }
    // Stderr flows to the daemon's own journal, tagged per process.
    command.spawn()
}

/// Read the slicer's stdout line by line for as long as it has one, parsing
/// each line as a `ProjectionStats` JSON object and publishing it. Runs on
/// its own thread because the alternative — polling a non-blocking pipe from
/// the reconciliation loop — would either busy-poll or add latency to every
/// other pass; a blocking read on a dedicated thread costs one thread for
/// the process's whole lifetime and nothing else.
///
/// Ends at EOF, which happens when the slicer exits; `sync_slicer` and
/// `shutdown` are what clear the snapshot in that case; this thread's job is
/// only to relay lines while there are any.
fn spawn_stats_reader(stdout: ChildStdout, snapshot: Arc<Snapshot>, events: EventHub) {
    let build = std::thread::Builder::new()
        .name("slicer-stats".to_string())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        tracing::debug!(%error, "slicer stdout reader stopped");
                        break;
                    }
                };
                match serde_json::from_str::<crate::model::ProjectionStats>(&line) {
                    Ok(stats) => {
                        snapshot.set_projection_stats(Some(stats.clone()));
                        events.publish(ServerEvent::ProjectionStatsChanged(Some(Box::new(stats))));
                    }
                    Err(error) => {
                        tracing::debug!(%error, line, "could not parse a slicer stats line");
                    }
                }
            }
        });
    if let Err(error) = build {
        tracing::warn!(%error, "could not start the slicer-stats reader thread");
    }
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
            rect: Rect::default(),
            pattern: None,
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
