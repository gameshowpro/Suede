//! Automatic output phase alignment.
//!
//! On the Wayland path a compositor session can start its heads several
//! milliseconds out of phase: on a four-head NVIDIA appliance every sway
//! restart measured 3.7–8.3 ms apart, costing 31–56 straddled frames per
//! ten-second interval, and the `output-phase` check's own fix (disable
//! every head, re-enable them together in one IPC message) brought them to
//! 0.04–0.16 ms on its first attempt in every one of more than forty runs —
//! see `docs/plans/display-component-results.md`. This module runs that fix
//! itself, once the slicer's own measurement says it is needed, instead of
//! waiting for an operator to press the button.
//!
//! The decision is [`Policy::evaluate`], pure so every rule is
//! table-testable: judge only fresh slicer intervals, act on two in a row
//! out of tolerance, at most [`MAX_ATTEMPTS`] times per compositor session,
//! and never in a direct session, during a pending reconcile, or without
//! stats. [`Aligner`] feeds it the snapshot and runs what it decides.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::checks::{ids, is_synthetic_output, phase_judgment, CheckRunner, PHASE_TOLERANCE_MS};
use crate::model::{
    OutputAlignmentResult, OutputAlignmentStatus, PresentationMode, ProjectionStats, SyncState,
};
use crate::reconciler::ReconcileTrigger;
use crate::snapshot::Snapshot;

/// Alignments allowed per compositor session. One has always been enough on
/// the hardware measured; the rest cover a head that was still settling.
pub const MAX_ATTEMPTS: u32 = 3;

/// Fresh out-of-tolerance intervals in a row before an alignment runs, so a
/// single disturbed interval (a mode-set, a hotplug) never blanks the wall.
pub const OUT_OF_PHASE_INTERVALS: u32 = 2;

/// How long a running slicer may go without reporting an interval before
/// alignment says, once per session, that it cannot judge the phase. Three
/// of the slicer's ten-second intervals.
pub const NO_STATS_GRACE: Duration = Duration::from_secs(30);

/// How often [`Aligner::run`] evaluates.
const POLL: Duration = Duration::from_secs(1);

/// What identifies one compositor session for the attempt budget: the Sway
/// socket and the set of active physical outputs. Either changing starts a
/// new budget.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionKey {
    pub socket: Option<PathBuf>,
    pub outputs: BTreeSet<String>,
}

/// Everything [`Policy::evaluate`] reads, gathered at one moment.
#[derive(Debug, Clone)]
pub struct Observation<'a> {
    pub now: Instant,
    /// Unix seconds, comparable with [`ProjectionStats::measured_at`].
    pub now_unix: u64,
    pub session: SessionKey,
    /// Whether this session presents directly rather than through Sway.
    pub direct: bool,
    /// Whether a reconcile pass is queued or in flight.
    pub reconcile_pending: bool,
    pub slicer_running: bool,
    /// The slicer's last reported interval, if any.
    pub stats: Option<&'a ProjectionStats>,
}

/// What [`Policy::evaluate`] concluded. Everything but `Wait` is logged.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing to do now.
    Wait,
    /// Run the `output-phase` fix; `phase_ms` is the largest absolute phase
    /// of the interval that decided it.
    Align { attempt: u32, phase_ms: f64 },
    /// The first fresh interval after an alignment.
    Judged {
        attempt: u32,
        phase_ms: f64,
        in_phase: bool,
    },
    /// Out of tolerance with every attempt used. Reported once per session.
    GaveUp { attempts: u32, phase_ms: f64 },
    /// The slicer is running but has reported nothing for
    /// [`NO_STATS_GRACE`]. Reported once per session.
    NotJudged,
}

/// The alignment policy's state for one daemon: the current session's
/// budget and what it has seen. See the module docs for the rules.
#[derive(Debug, Clone)]
pub struct Policy {
    enabled: bool,
    session: Option<SessionKey>,
    /// When the slicer last gave this session something to go on: the
    /// session's start, the last fresh interval, or the last alignment.
    last_progress: Option<Instant>,
    /// `measured_at` of the newest interval already judged, so the same
    /// report is never counted twice.
    last_judged: Option<u64>,
    /// Intervals count only if they started at or after this Unix second —
    /// set by an alignment (its own disturbance must not be judged) and by
    /// a session change (the old outputs' interval says nothing about the
    /// new ones).
    fresh_after: Option<u64>,
    /// An alignment ran and its first fresh interval has not arrived yet.
    awaiting: bool,
    consecutive_out: u32,
    gave_up_reported: bool,
    not_judged_reported: bool,
    status: OutputAlignmentStatus,
}

impl Policy {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            session: None,
            last_progress: None,
            last_judged: None,
            fresh_after: None,
            awaiting: false,
            consecutive_out: 0,
            gave_up_reported: false,
            not_judged_reported: false,
            status: OutputAlignmentStatus {
                enabled,
                ..OutputAlignmentStatus::default()
            },
        }
    }

    /// What `GET /system` reports as `outputAlignment`.
    pub fn status(&self) -> &OutputAlignmentStatus {
        &self.status
    }

    /// Decide what to do about `observation`.
    pub fn evaluate(&mut self, observation: &Observation<'_>) -> Decision {
        if !self.enabled || observation.direct || observation.reconcile_pending {
            return Decision::Wait;
        }
        // Fewer than two heads have no phase to share. Such a set is also
        // what an alignment's own disable looks like part-way through, so it
        // never starts a new session either.
        if observation.session.outputs.len() < 2 {
            return Decision::Wait;
        }
        // An alignment's disable/enable can show transient output sets; the
        // session is only re-read once its result is in.
        if !self.awaiting && self.session.as_ref() != Some(&observation.session) {
            let first = self.session.is_none();
            self.start_session(observation, first);
        }

        let fresh = observation.stats.filter(|stats| self.is_fresh(stats));
        let Some(stats) = fresh else {
            return self.without_stats(observation);
        };
        self.last_judged = Some(stats.measured_at);
        self.last_progress = Some(observation.now);
        let Some(judgment) = phase_judgment(stats) else {
            // Nothing to compare (no presentation timing, or one head
            // reporting): neither in nor out of phase.
            return Decision::Wait;
        };
        self.status.last_phase_ms = Some(judgment.max_abs_ms);
        let after_alignment = std::mem::take(&mut self.awaiting);
        if after_alignment {
            self.fresh_after = None;
        }

        if !judgment.out_of_phase {
            self.consecutive_out = 0;
            self.gave_up_reported = false;
            self.status.last_result = Some(if self.status.attempts == 0 {
                OutputAlignmentResult::InPhase
            } else {
                OutputAlignmentResult::Aligned
            });
            return if after_alignment {
                Decision::Judged {
                    attempt: self.status.attempts,
                    phase_ms: judgment.max_abs_ms,
                    in_phase: true,
                }
            } else {
                Decision::Wait
            };
        }

        self.consecutive_out += 1;
        if after_alignment {
            self.status.last_result = Some(OutputAlignmentResult::StillOutOfPhase);
            return Decision::Judged {
                attempt: self.status.attempts,
                phase_ms: judgment.max_abs_ms,
                in_phase: false,
            };
        }
        if self.consecutive_out < OUT_OF_PHASE_INTERVALS {
            if self.status.last_result != Some(OutputAlignmentResult::StillOutOfPhase) {
                self.status.last_result = Some(OutputAlignmentResult::OutOfPhase);
            }
            return Decision::Wait;
        }
        if self.status.attempts >= MAX_ATTEMPTS {
            self.status.last_result = Some(OutputAlignmentResult::GaveUp);
            if self.gave_up_reported {
                return Decision::Wait;
            }
            self.gave_up_reported = true;
            return Decision::GaveUp {
                attempts: self.status.attempts,
                phase_ms: judgment.max_abs_ms,
            };
        }
        self.status.attempts += 1;
        self.consecutive_out = 0;
        Decision::Align {
            attempt: self.status.attempts,
            phase_ms: judgment.max_abs_ms,
        }
    }

    /// Record how the alignment [`Self::evaluate`] asked for went. Only
    /// intervals that start after `now_unix` will judge it.
    pub fn alignment_finished(&mut self, now: Instant, now_unix: u64, succeeded: bool) {
        self.last_progress = Some(now);
        if succeeded {
            self.awaiting = true;
            self.fresh_after = Some(now_unix);
        } else {
            self.status.last_result = Some(OutputAlignmentResult::Failed);
        }
    }

    fn start_session(&mut self, observation: &Observation<'_>, first: bool) {
        self.session = Some(observation.session.clone());
        self.last_progress = Some(observation.now);
        // The daemon's first session has no older outputs whose interval
        // could be mistaken for the current ones; a later one does.
        self.fresh_after = (!first).then_some(observation.now_unix);
        self.consecutive_out = 0;
        self.gave_up_reported = false;
        self.not_judged_reported = false;
        self.status = OutputAlignmentStatus {
            enabled: self.enabled,
            ..OutputAlignmentStatus::default()
        };
    }

    fn is_fresh(&self, stats: &ProjectionStats) -> bool {
        if self
            .last_judged
            .is_some_and(|judged| stats.measured_at <= judged)
        {
            return false;
        }
        // `measured_at` is the interval's end, truncated to the second, so
        // this start is never later than the true one.
        let started = stats.measured_at as f64 - stats.interval_seconds;
        self.fresh_after.is_none_or(|after| started >= after as f64)
    }

    fn without_stats(&mut self, observation: &Observation<'_>) -> Decision {
        if !observation.slicer_running || self.not_judged_reported {
            return Decision::Wait;
        }
        let since = self.last_progress.unwrap_or(observation.now);
        if observation.now.saturating_duration_since(since) < NO_STATS_GRACE {
            return Decision::Wait;
        }
        self.not_judged_reported = true;
        if self.status.last_result.is_none() {
            self.status.last_result = Some(OutputAlignmentResult::NotJudged);
        }
        Decision::NotJudged
    }
}

/// The sentence the `output-phase` check appends while automatic alignment
/// is on and could act (never in a direct session).
pub fn check_note(status: &OutputAlignmentStatus) -> String {
    let mut note = format!(
        "Automatic alignment is on: Suede re-aligns the displays itself after \
         {OUT_OF_PHASE_INTERVALS} slicer intervals in a row more than \
         {PHASE_TOLERANCE_MS:.1} ms apart, at most {MAX_ATTEMPTS} times per \
         compositor session ({} so far).",
        status.attempts
    );
    match status.last_result {
        Some(OutputAlignmentResult::GaveUp) => note.push_str(
            " It has used every attempt this session without bringing them into \
             phase, so it will not try again until the compositor restarts or the \
             displays change.",
        ),
        Some(OutputAlignmentResult::Failed) => {
            note.push_str(" Its last attempt failed; see the daemon's log.")
        }
        Some(OutputAlignmentResult::NotJudged) => note.push_str(
            " It cannot judge the phase while the content is static, because the \
             slicer reports nothing until something draws.",
        ),
        _ => {}
    }
    note
}

/// Feeds [`Policy`] from the live snapshot and runs the `output-phase` fix
/// when it says so.
pub struct Aligner {
    checks: Arc<CheckRunner>,
    snapshot: Arc<Snapshot>,
    trigger: ReconcileTrigger,
    /// How the current Sway socket is found; a function so tests need not
    /// depend on whatever sway happens to be running on the test machine.
    socket: fn() -> Option<PathBuf>,
    policy: Policy,
}

impl Aligner {
    pub fn new(
        enabled: bool,
        checks: Arc<CheckRunner>,
        snapshot: Arc<Snapshot>,
        trigger: ReconcileTrigger,
    ) -> Self {
        Self {
            checks,
            snapshot,
            trigger,
            socket: crate::sway::discover_socket,
            policy: Policy::new(enabled),
        }
    }

    /// The same aligner with a fixed socket lookup, for tests.
    #[cfg(test)]
    pub(crate) fn with_socket(mut self, socket: fn() -> Option<PathBuf>) -> Self {
        self.socket = socket;
        self
    }

    /// Evaluate once, run an alignment if one is due, and publish the
    /// status. Returns what was decided.
    pub async fn step(&mut self) -> Decision {
        let stats = self.snapshot.projection_stats();
        let observation = Observation {
            now: Instant::now(),
            now_unix: unix_now(),
            session: SessionKey {
                socket: (self.socket)(),
                outputs: self
                    .snapshot
                    .outputs()
                    .into_iter()
                    .filter(|output| output.active && !is_synthetic_output(&output.name))
                    .map(|output| output.name)
                    .collect(),
            },
            direct: self
                .snapshot
                .presentation()
                .is_some_and(|status| status.effective == PresentationMode::Direct),
            reconcile_pending: self.trigger.is_pending()
                || self.snapshot.status().state == SyncState::Reconciling,
            slicer_running: self.snapshot.slicer_running(),
            stats: stats.as_ref(),
        };
        let decision = self.policy.evaluate(&observation);
        match &decision {
            Decision::Wait => {}
            Decision::Align { attempt, phase_ms } => {
                tracing::info!(
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    phase_before_ms = phase_ms,
                    "displays out of phase for {OUT_OF_PHASE_INTERVALS} intervals (up to \
                     {phase_ms:.2} ms); aligning them, attempt {attempt} of {MAX_ATTEMPTS}"
                );
                let result = self.checks.fix(ids::OUTPUT_PHASE).await;
                let succeeded = result.is_ok();
                match result {
                    Ok(detail) => tracing::info!(attempt, %detail, "output alignment ran"),
                    Err(error) => tracing::warn!(attempt, %error, "output alignment failed"),
                }
                self.policy
                    .alignment_finished(Instant::now(), unix_now(), succeeded);
            }
            Decision::Judged {
                attempt,
                phase_ms,
                in_phase,
            } => {
                if *in_phase {
                    tracing::info!(
                        attempt,
                        phase_after_ms = phase_ms,
                        "displays aligned: within {phase_ms:.2} ms after attempt {attempt}"
                    );
                } else {
                    tracing::warn!(
                        attempt,
                        phase_after_ms = phase_ms,
                        "displays still out of phase after attempt {attempt}: up to \
                         {phase_ms:.2} ms"
                    );
                }
            }
            Decision::GaveUp { attempts, phase_ms } => tracing::warn!(
                attempts,
                phase_ms,
                "displays still out of phase (up to {phase_ms:.2} ms) after all {attempts} \
                 alignment attempts this session; not trying again until the compositor \
                 or the displays change"
            ),
            Decision::NotJudged => tracing::info!(
                "cannot judge output phase: the slicer is running but has reported no \
                 frames, which is what static content looks like"
            ),
        }
        self.snapshot
            .set_output_alignment(self.policy.status().clone());
        decision
    }

    /// Evaluate once a second until shutdown. A poll rather than a
    /// subscription to `projection_stats_changed`: that event also carries
    /// every live-control acknowledgment, which can arrive many times a
    /// second while someone drags a corner, and a second's latency is
    /// nothing against the slicer's ten-second interval.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        self.snapshot
            .set_output_alignment(self.policy.status().clone());
        if !self.policy.enabled {
            tracing::info!("automatic output alignment is off (align_outputs = false)");
            return;
        }
        let mut poll = tokio::time::interval(POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = poll.tick() => {
                    self.step().await;
                }
            }
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CaptureIntervals, FrameCost, LagFrames, OutputTiming};

    /// One slicer interval ending at `measured_at`, with these phases (the
    /// first is the reference, 0.0 by construction in real reports).
    fn stats(measured_at: u64, phases: &[f64]) -> ProjectionStats {
        ProjectionStats {
            measured_at,
            interval_seconds: 10.0,
            free_run: false,
            canvas_fps: 60.0,
            presented_fps: 60.0,
            frames_superseded: 0,
            stalls: 0,
            per_frame_ms: FrameCost {
                waiting: 1.0,
                snapshot: 1.0,
                requesting: 1.0,
                blending: 1.0,
                gpu: 0.0,
            },
            presentation_feedback: true,
            offset_ms: None,
            straddles: 0,
            gate_holds: 0,
            renderer: "gpu".to_string(),
            presentation_backend: None,
            timestamp_source: None,
            capture_intervals: CaptureIntervals::default(),
            outputs: phases
                .iter()
                .enumerate()
                .map(|(index, phase)| OutputTiming {
                    name: format!("DP-{}", index + 1),
                    presented: 600,
                    discarded: 0,
                    zero_copy_presented: 0,
                    refresh_hz: Some(60.0),
                    phase_ms: Some(*phase),
                    phase_spread_ms: Some(0.1),
                    lag_frames: LagFrames::default(),
                })
                .collect(),
        }
    }

    const OUT: &[f64] = &[0.0, 6.2, 6.3, 6.2];
    const IN: &[f64] = &[0.0, 0.1, -0.05, 0.08];

    fn session(outputs: &[&str]) -> SessionKey {
        SessionKey {
            socket: Some(PathBuf::from("/run/user/1000/sway-ipc.1000.1.sock")),
            outputs: outputs.iter().map(|name| name.to_string()).collect(),
        }
    }

    fn four() -> SessionKey {
        session(&["DP-1", "DP-2", "DP-3", "DP-4"])
    }

    /// A Wayland session with four heads, nothing pending, slicer running.
    struct Clock {
        start: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
            }
        }

        /// Observe at `seconds` after the start (Unix second 1000 + that).
        fn at<'a>(&self, seconds: u64, stats: Option<&'a ProjectionStats>) -> Observation<'a> {
            Observation {
                now: self.start + Duration::from_secs(seconds),
                now_unix: 1000 + seconds,
                session: four(),
                direct: false,
                reconcile_pending: false,
                slicer_running: true,
                stats,
            }
        }
    }

    #[test]
    fn two_consecutive_out_of_phase_intervals_start_one_alignment() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        let first = stats(1010, OUT);
        assert_eq!(policy.evaluate(&clock.at(10, Some(&first))), Decision::Wait);
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::OutOfPhase)
        );
        // The same report again is not a second interval.
        assert_eq!(policy.evaluate(&clock.at(11, Some(&first))), Decision::Wait);
        let second = stats(1020, OUT);
        assert_eq!(
            policy.evaluate(&clock.at(20, Some(&second))),
            Decision::Align {
                attempt: 1,
                phase_ms: 6.3
            }
        );
        assert_eq!(policy.status().attempts, 1);
        assert_eq!(policy.status().last_phase_ms, Some(6.3));
    }

    #[test]
    fn an_in_phase_interval_resets_the_count() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(10, Some(&stats(1010, OUT))));
        assert_eq!(
            policy.evaluate(&clock.at(20, Some(&stats(1020, IN)))),
            Decision::Wait
        );
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::InPhase)
        );
        assert_eq!(
            policy.evaluate(&clock.at(30, Some(&stats(1030, OUT)))),
            Decision::Wait,
            "one out-of-phase interval after an in-phase one is not two in a row"
        );
    }

    #[test]
    fn exactly_the_tolerance_is_in_phase() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        let edge = [0.0, PHASE_TOLERANCE_MS, -PHASE_TOLERANCE_MS];
        policy.evaluate(&clock.at(10, Some(&stats(1010, &edge))));
        policy.evaluate(&clock.at(20, Some(&stats(1020, &edge))));
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::InPhase)
        );
        assert_eq!(policy.status().attempts, 0);
    }

    #[test]
    fn the_result_is_judged_only_on_an_interval_that_started_after_the_alignment() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(10, Some(&stats(1010, OUT))));
        policy.evaluate(&clock.at(20, Some(&stats(1020, OUT))));
        policy.alignment_finished(clock.start + Duration::from_secs(23), 1023, true);

        // Ends after the alignment but started before it: half of it is the
        // alignment's own disturbance.
        let straddling = stats(1030, OUT);
        assert_eq!(
            policy.evaluate(&clock.at(30, Some(&straddling))),
            Decision::Wait
        );
        let after = stats(1040, IN);
        assert_eq!(
            policy.evaluate(&clock.at(40, Some(&after))),
            Decision::Judged {
                attempt: 1,
                phase_ms: 0.1,
                in_phase: true
            }
        );
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::Aligned)
        );
        // Later in-phase intervals keep saying aligned, not in phase: this
        // session needed an alignment.
        assert_eq!(
            policy.evaluate(&clock.at(50, Some(&stats(1050, IN)))),
            Decision::Wait
        );
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::Aligned)
        );
    }

    #[test]
    fn at_most_three_attempts_per_session_then_one_give_up() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        let mut t = 10;
        let mut next = |policy: &mut Policy| {
            let report = stats(1000 + t, OUT);
            let decision = policy.evaluate(&clock.at(t, Some(&report)));
            t += 10;
            decision
        };
        let mut aligns = 0;
        let mut gave_up = 0;
        for _ in 0..20 {
            match next(&mut policy) {
                Decision::Align { .. } => {
                    aligns += 1;
                    // Finish just before the next interval starts counting.
                    let now = policy.last_progress.unwrap();
                    policy.alignment_finished(now, policy.last_judged.unwrap(), true);
                }
                Decision::GaveUp { attempts, .. } => {
                    assert_eq!(attempts, MAX_ATTEMPTS);
                    gave_up += 1;
                }
                _ => {}
            }
        }
        assert_eq!(aligns, MAX_ATTEMPTS);
        assert_eq!(gave_up, 1, "giving up is reported once");
        assert_eq!(policy.status().attempts, MAX_ATTEMPTS);
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::GaveUp)
        );
    }

    #[test]
    fn a_failed_alignment_still_counts_and_is_reported() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(10, Some(&stats(1010, OUT))));
        policy.evaluate(&clock.at(20, Some(&stats(1020, OUT))));
        policy.alignment_finished(clock.start + Duration::from_secs(21), 1021, false);
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::Failed)
        );
        assert_eq!(policy.status().attempts, 1);
        // Not awaiting a result: the next two out-of-phase intervals try again.
        policy.evaluate(&clock.at(30, Some(&stats(1030, OUT))));
        assert_eq!(
            policy.evaluate(&clock.at(40, Some(&stats(1040, OUT)))),
            Decision::Align {
                attempt: 2,
                phase_ms: 6.3
            }
        );
    }

    #[test]
    fn a_new_socket_or_output_set_resets_the_budget() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(10, Some(&stats(1010, OUT))));
        policy.evaluate(&clock.at(20, Some(&stats(1020, OUT))));
        policy.alignment_finished(clock.start + Duration::from_secs(21), 1021, true);
        policy.evaluate(&clock.at(40, Some(&stats(1040, IN))));
        assert_eq!(policy.status().attempts, 1);

        // A projector unplugged: a new session, and the interval measured on
        // the old set is not judged against the new one.
        let replugged = stats(1045, OUT);
        let mut observation = clock.at(45, Some(&replugged));
        observation.session = session(&["DP-1", "DP-2", "DP-3"]);
        assert_eq!(policy.evaluate(&observation), Decision::Wait);
        assert_eq!(policy.status().attempts, 0);
        assert_eq!(policy.status().last_result, None);

        // A compositor restart on the same outputs is a new session too.
        let three = session(&["DP-1", "DP-2", "DP-3"]);
        for t in [60, 70] {
            let report = stats(1000 + t, OUT);
            let mut observation = clock.at(t, Some(&report));
            observation.session = three.clone();
            policy.evaluate(&observation);
        }
        assert_eq!(policy.status().attempts, 1);
        let mut observation = clock.at(75, None);
        observation.session = SessionKey {
            socket: Some(PathBuf::from("/run/user/1000/sway-ipc.1000.2.sock")),
            ..three
        };
        policy.evaluate(&observation);
        assert_eq!(policy.status().attempts, 0);
    }

    #[test]
    fn transient_output_sets_during_an_alignment_do_not_reset_the_budget() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(10, Some(&stats(1010, OUT))));
        policy.evaluate(&clock.at(20, Some(&stats(1020, OUT))));
        policy.alignment_finished(clock.start + Duration::from_secs(21), 1021, true);

        // Every head disabled, then two of four back: neither is a session.
        for outputs in [&[][..], &["DP-1", "DP-2"][..]] {
            let mut observation = clock.at(22, None);
            observation.session = session(outputs);
            assert_eq!(policy.evaluate(&observation), Decision::Wait);
        }
        assert_eq!(policy.status().attempts, 1);
        policy.evaluate(&clock.at(40, Some(&stats(1040, IN))));
        assert_eq!(policy.status().attempts, 1);
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::Aligned)
        );
    }

    #[test]
    fn never_in_a_direct_session_during_a_pending_reconcile_or_when_disabled() {
        let clock = Clock::new();
        let out_a = stats(1010, OUT);
        let out_b = stats(1020, OUT);

        let mut direct = Policy::new(true);
        for (t, report) in [(10, &out_a), (20, &out_b)] {
            let mut observation = clock.at(t, Some(report));
            observation.direct = true;
            assert_eq!(direct.evaluate(&observation), Decision::Wait);
        }
        assert_eq!(direct.status().last_result, None);

        let mut disabled = Policy::new(false);
        assert_eq!(
            disabled.evaluate(&clock.at(10, Some(&out_a))),
            Decision::Wait
        );
        assert_eq!(
            disabled.evaluate(&clock.at(20, Some(&out_b))),
            Decision::Wait
        );
        assert!(!disabled.status().enabled);

        // A pending reconcile holds the decision rather than dropping it:
        // the same interval is judged once the pass is done.
        let mut pending = Policy::new(true);
        pending.evaluate(&clock.at(10, Some(&out_a)));
        let mut observation = clock.at(20, Some(&out_b));
        observation.reconcile_pending = true;
        assert_eq!(pending.evaluate(&observation), Decision::Wait);
        assert!(matches!(
            pending.evaluate(&clock.at(21, Some(&out_b))),
            Decision::Align { attempt: 1, .. }
        ));
    }

    #[test]
    fn fewer_than_two_heads_is_never_judged() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        for t in [10, 20, 30] {
            let report = stats(1000 + t, OUT);
            let mut observation = clock.at(t, Some(&report));
            observation.session = session(&["DP-1"]);
            assert_eq!(policy.evaluate(&observation), Decision::Wait);
        }
    }

    #[test]
    fn static_content_is_reported_once_and_never_aligned() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        assert_eq!(policy.evaluate(&clock.at(0, None)), Decision::Wait);
        assert_eq!(policy.evaluate(&clock.at(29, None)), Decision::Wait);
        assert_eq!(policy.evaluate(&clock.at(30, None)), Decision::NotJudged);
        for t in 31..200 {
            assert_eq!(policy.evaluate(&clock.at(t, None)), Decision::Wait);
        }
        assert_eq!(policy.status().attempts, 0);
        assert_eq!(
            policy.status().last_result,
            Some(OutputAlignmentResult::NotJudged)
        );

        // A stale report from before the content went still is the same as
        // none: it was already judged.
        let mut policy = Policy::new(true);
        let old = stats(1010, OUT);
        policy.evaluate(&clock.at(10, Some(&old)));
        for t in 11..100 {
            assert_ne!(
                policy.evaluate(&clock.at(t, Some(&old))),
                Decision::Align {
                    attempt: 1,
                    phase_ms: 6.3
                }
            );
        }
        assert_eq!(policy.status().attempts, 0);

        // No slicer at all (a tiling appliance) says nothing.
        let mut policy = Policy::new(true);
        let mut observation = clock.at(100, None);
        observation.slicer_running = false;
        policy.evaluate(&clock.at(0, None));
        assert_eq!(policy.evaluate(&observation), Decision::Wait);
    }

    #[test]
    fn an_interval_with_nothing_to_compare_is_neither() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        let mut no_timing = stats(1010, OUT);
        no_timing.presentation_feedback = false;
        policy.evaluate(&clock.at(10, Some(&no_timing)));
        let mut no_timing = stats(1020, OUT);
        no_timing.presentation_feedback = false;
        assert_eq!(
            policy.evaluate(&clock.at(20, Some(&no_timing))),
            Decision::Wait
        );
        assert_eq!(policy.status().last_result, None);
        assert_eq!(policy.status().attempts, 0);
    }

    #[test]
    fn the_check_note_names_the_rule_and_the_outcome() {
        let mut status = OutputAlignmentStatus {
            enabled: true,
            attempts: 1,
            last_result: Some(OutputAlignmentResult::Aligned),
            last_phase_ms: Some(0.1),
        };
        let note = check_note(&status);
        assert!(note.starts_with("Automatic alignment is on"), "{note}");
        assert!(note.contains("1.0 ms"), "{note}");
        assert!(note.contains("(1 so far)"), "{note}");
        status.last_result = Some(OutputAlignmentResult::GaveUp);
        assert!(check_note(&status).contains("used every attempt"));
        status.last_result = Some(OutputAlignmentResult::NotJudged);
        assert!(check_note(&status).contains("static"));
    }

    /// End to end against the mock compositor: a misaligned report, twice,
    /// becomes exactly one disable/re-enable of both heads and one
    /// reconcile request — and nothing more while its result is pending.
    #[tokio::test]
    async fn a_misaligned_report_leads_to_exactly_one_fix_and_a_reconcile() {
        use crate::audio::mock::MockAudio;
        use crate::checks::CheckRunnerDeps;
        use crate::config::BootstrapConfig;
        use crate::events::EventHub;
        use crate::model::{OutputConfig, PresentationStatus};
        use crate::supervisor::Supervisor;
        use crate::sway::{mock::MockSway, SwayClient};

        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            sway_config_path: dir.path().join("sway/config"),
            systemd_user_dir: dir.path().join("systemd/user"),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::with_fixtures());
        let store = Arc::new(crate::state::StateStore::ephemeral(
            dir.path().to_path_buf(),
        ));
        store
            .update(|state| {
                for name in ["HDMI-A-1", "HDMI-A-2"] {
                    let mut config = OutputConfig::new(crate::model::OutputMatch::by_name(name));
                    config.mode = Some(crate::model::Mode {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60.0,
                    });
                    state.outputs.push(config);
                }
            })
            .unwrap();
        let snapshot = Arc::new(Snapshot::new());
        snapshot.set_outputs(sway.get_outputs().await.unwrap());
        snapshot.set_presentation(PresentationStatus {
            requested: PresentationMode::Wayland,
            effective: PresentationMode::Wayland,
            reason: None,
            outputs: Vec::new(),
        });
        snapshot.set_slicer_running(true);
        let (trigger, mut receiver) = crate::reconciler::Reconciler::channel();
        let checks = Arc::new(CheckRunner::new(CheckRunnerDeps {
            bootstrap,
            sway: sway.clone(),
            audio: Arc::new(MockAudio::default()),
            store,
            events: EventHub::new(),
            capabilities: Arc::new(crate::capabilities::CapabilityStore::new(dir.path())),
            snapshot: snapshot.clone(),
            trigger: trigger.clone(),
            supervisor: Arc::new(Supervisor::idle(dir.path())),
        }));
        let mut aligner =
            Aligner::new(true, checks, snapshot.clone(), trigger).with_socket(|| None);

        let misaligned = |measured_at| {
            let mut report = stats(measured_at, &[0.0, 7.1]);
            report.outputs[0].name = "HDMI-A-1".into();
            report.outputs[1].name = "HDMI-A-2".into();
            report
        };
        let now = unix_now();
        snapshot.set_projection_stats(Some(misaligned(now - 10)));
        assert_eq!(aligner.step().await, Decision::Wait);
        assert!(sway.commands().is_empty(), "one interval is not enough");

        snapshot.set_projection_stats(Some(misaligned(now)));
        assert_eq!(
            aligner.step().await,
            Decision::Align {
                attempt: 1,
                phase_ms: 7.1
            }
        );
        let disables = |sway: &MockSway| {
            sway.commands()
                .iter()
                .filter(|command| command.ends_with(" disable"))
                .count()
        };
        assert_eq!(disables(&sway), 2, "{:?}", sway.commands());
        for name in ["HDMI-A-1", "HDMI-A-2"] {
            assert!(
                sway.commands()
                    .iter()
                    .any(|command| command == &format!("output {name} enable")),
                "{:?}",
                sway.commands()
            );
        }
        assert_eq!(receiver.try_recv().ok(), Some("output-phase fix"));
        assert_eq!(snapshot.output_alignment().attempts, 1);

        // Its own reconcile has been taken; the same report again, and more
        // polls, are no reason to run it a second time.
        for _ in 0..3 {
            assert_eq!(aligner.step().await, Decision::Wait);
        }
        assert_eq!(disables(&sway), 2, "{:?}", sway.commands());
        assert!(receiver.try_recv().is_err());
    }
}
