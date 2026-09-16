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
use crate::model::{Divergence, ProjectionReport};
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

    /// Whether the slicer child is alive right now.
    ///
    /// `sync_slicer` already reaps it with `try_wait` before touching
    /// anything else, so this is never a guess — which is the whole point:
    /// a bench on 2026-09-15 had `GET /projection/stats` and the
    /// `output-phase` check both call a running slicer "not running"
    /// because neither had anything better than "have stats arrived" to go
    /// on, and a damage-driven frame loop reports nothing for a static
    /// page. See `ProjectionReport`.
    pub fn slicer_running(&self) -> bool {
        self.slicer.is_some()
    }

    /// The running slicer's process id, if any — only ever needed to tell
    /// "the same process" from "a fresh one" in a test, since no production
    /// caller has a use for the number itself.
    #[cfg(test)]
    pub(crate) fn slicer_pid(&self) -> Option<u32> {
        self.slicer.as_ref().map(|running| running.child.id())
    }

    /// Clear the last-reported projection stats and liveness together, and
    /// tell any SSE listener, but only if something actually changed — a
    /// manager that never ran a slicer must not spam a `false`/`null` on
    /// every reconciliation pass.
    ///
    /// The two are cleared from this one place rather than separately,
    /// because a stopped slicer's last interval is stale the instant it
    /// stops: a reader that saw `running: true` with an interval left over
    /// from ten seconds ago, while the process was already dead, would be
    /// exactly the ambiguity this file exists to remove.
    fn clear_projection_stats(&self) {
        let stats_changed = self.snapshot.set_projection_stats(None);
        let running_changed = self.snapshot.set_slicer_running(false);
        if stats_changed || running_changed {
            self.events
                .publish(ServerEvent::ProjectionStatsChanged(Box::new(
                    ProjectionReport {
                        running: false,
                        last_interval: None,
                    },
                )));
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

    /// Force the next `sync_slicer` call to respawn the slicer even if its
    /// spec is unchanged. A no-op when none is running.
    ///
    /// Cheap insurance for a teardown `sync_slicer`'s own fingerprint diff
    /// cannot see: when an output disappears and comes back under the same
    /// name, the geometry `SlicerSpec` is built from ends up identical to
    /// before, so its fingerprint does not change and an unmodified
    /// `sync_slicer` would leave the existing (dead) slicer running —
    /// exactly the four-projector-bench defect this exists to close. The
    /// reconciler calls this before `sync_slicer` on any pass whose
    /// `OutputPlan::topology_changed` is true, i.e. one that actually issued
    /// enable/disable commands, so a compositor that does not remove the
    /// `wl_output` global on its own (this one's own self-heal in
    /// `projection::slicer` is the other route, for one that does) still
    /// gets a fresh slicer bound against the outputs that exist now.
    pub fn restart_slicer(&mut self) {
        if let Some(mut old) = self.slicer.take() {
            tracing::info!("restarting the slicer: output topology changed");
            let _ = old.child.kill();
            let _ = old.child.wait();
            self.clear_projection_stats();
        }
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
                        // A stats line only ever arrives from a live
                        // process, so this is as good a place as any to
                        // affirm liveness too — the reconciler sets it after
                        // every `sync_slicer` call, but a client that only
                        // watches events should not have to wait for the
                        // next reconciliation pass to hear it.
                        snapshot.set_projection_stats(Some(stats.clone()));
                        snapshot.set_slicer_running(true);
                        events.publish(ServerEvent::ProjectionStatsChanged(Box::new(
                            ProjectionReport {
                                running: true,
                                last_interval: Some(stats),
                            },
                        )));
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

    // --- restart_slicer: forcing a respawn the fingerprint cannot see ----
    //
    // `sync_slicer` re-execs this very test binary as `suede slice`, which
    // does not understand that subcommand and exits (with an error)
    // virtually as soon as it starts — real enough to exercise process
    // bookkeeping, but too short-lived to stand in for a genuinely running
    // slicer across any real time gap. Every test below therefore makes its
    // two `sync_slicer`/`restart_slicer` calls back-to-back with no `.await`
    // or sleep in between, so they run in the same thread well inside the
    // minimum time a freshly exec'd process needs to even start, let alone
    // exit — unlike a reconciler-level test spanning a whole extra pass (or
    // the 3-second output-settle sleep), which cannot tell a forced restart
    // apart from the fake process simply having crashed on its own by then.

    fn minimal_slicer_spec() -> SlicerSpec {
        SlicerSpec {
            source: "HEADLESS-1".to_string(),
            canvas_width: 100,
            canvas_height: 100,
            gamma: 2.2,
            black_lift: 0.0,
            pattern: None,
            free_run: false,
            renderer: crate::model::Renderer::Cpu,
            slices: Vec::new(),
        }
    }

    fn manager_for_test() -> BlendManager {
        BlendManager::new(Arc::new(Snapshot::new()), EventHub::new())
    }

    #[test]
    fn an_unforced_call_with_an_unchanged_spec_leaves_the_slicer_alone() {
        let mut manager = manager_for_test();
        let spec = minimal_slicer_spec();

        manager.sync_slicer(Some(&spec));
        let pid = manager.slicer_pid().expect("must have spawned a slicer");

        manager.sync_slicer(Some(&spec));
        assert_eq!(
            manager.slicer_pid(),
            Some(pid),
            "an unchanged spec must not be restarted"
        );
    }

    #[test]
    fn restart_slicer_forces_a_respawn_even_with_an_unchanged_spec() {
        // This is exactly what the reconciler calls before `sync_slicer` on
        // any pass whose `OutputPlan::topology_changed` is true — see
        // `restart_slicer`'s own doc for why the spec alone cannot be
        // trusted to notice an output that disappeared and came back under
        // the same name.
        let mut manager = manager_for_test();
        let spec = minimal_slicer_spec();

        manager.sync_slicer(Some(&spec));
        let pid = manager.slicer_pid().expect("must have spawned a slicer");

        manager.restart_slicer();
        manager.sync_slicer(Some(&spec));
        assert_ne!(
            manager.slicer_pid(),
            Some(pid),
            "a forced restart must respawn even an unchanged spec"
        );
    }

    #[test]
    fn restart_slicer_is_a_no_op_when_nothing_is_running() {
        let mut manager = manager_for_test();
        manager.restart_slicer();
        assert!(!manager.slicer_running());
    }
}
