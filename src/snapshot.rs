//! The live view of the world, shared between the reconciler and the API.
//!
//! Everything here is re-derived from Sway and PipeWire; none of it is persisted.

use std::sync::RwLock;

use crate::model::{Output, ProjectionReport, ProjectionStats, Status, Window};

#[derive(Default)]
pub struct Snapshot {
    outputs: RwLock<Vec<Output>>,
    windows: RwLock<Vec<Window>>,
    status: RwLock<Status>,
    /// What the slicer last reported, if one is running. Set by the manager's
    /// reader thread from the slicer's stdout; cleared when the slicer stops
    /// or is found to have exited (see `BlendManager`).
    projection_stats: RwLock<Option<ProjectionStats>>,
    /// Whether a slicer process is alive right now. `BlendManager` already
    /// reaps it with `try_wait`, so it is the one source of truth; this is
    /// just where that fact is published for the API and the health check
    /// to read, instead of each guessing it from whether stats have arrived
    /// (see `ProjectionReport` for why that guess was wrong on 2026-09-15).
    slicer_running: RwLock<bool>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.outputs.read().unwrap().clone()
    }

    pub fn output(&self, name: &str) -> Option<Output> {
        self.outputs
            .read()
            .unwrap()
            .iter()
            .find(|output| output.name == name)
            .cloned()
    }

    /// Replace the output list, reporting whether it actually changed.
    pub fn set_outputs(&self, outputs: Vec<Output>) -> bool {
        let mut guard = self.outputs.write().unwrap();
        if *guard == outputs {
            return false;
        }
        *guard = outputs;
        true
    }

    pub fn windows(&self) -> Vec<Window> {
        self.windows.read().unwrap().clone()
    }

    /// Replace the window list, reporting whether it actually changed.
    pub fn set_windows(&self, windows: Vec<Window>) -> bool {
        let mut guard = self.windows.write().unwrap();
        if *guard == windows {
            return false;
        }
        *guard = windows;
        true
    }

    pub fn status(&self) -> Status {
        self.status.read().unwrap().clone()
    }

    /// Replace the status, reporting whether it actually changed.
    pub fn set_status(&self, status: Status) -> bool {
        let mut guard = self.status.write().unwrap();
        if *guard == status {
            return false;
        }
        *guard = status;
        true
    }

    pub fn projection_stats(&self) -> Option<ProjectionStats> {
        self.projection_stats.read().unwrap().clone()
    }

    /// Replace the projection stats, reporting whether they actually changed.
    pub fn set_projection_stats(&self, stats: Option<ProjectionStats>) -> bool {
        let mut guard = self.projection_stats.write().unwrap();
        if *guard == stats {
            return false;
        }
        *guard = stats;
        true
    }

    pub fn slicer_running(&self) -> bool {
        *self.slicer_running.read().unwrap()
    }

    /// Replace the slicer's liveness, reporting whether it actually changed.
    pub fn set_slicer_running(&self, running: bool) -> bool {
        let mut guard = self.slicer_running.write().unwrap();
        if *guard == running {
            return false;
        }
        *guard = running;
        true
    }

    /// What `GET /projection/stats` and `projection_stats_changed` serve:
    /// liveness alongside whatever the slicer has most recently reported.
    pub fn projection_report(&self) -> ProjectionReport {
        ProjectionReport {
            running: self.slicer_running(),
            last_interval: self.projection_stats(),
        }
    }

    /// Total height of the layout, used to park the cursor below every output.
    pub fn layout_height(&self) -> i32 {
        self.outputs
            .read()
            .unwrap()
            .iter()
            .filter(|output| output.active)
            .map(|output| output.rect.y + output.rect.height)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Rect, SyncState};

    fn output(name: &str, y: i32, height: i32) -> Output {
        Output {
            name: name.into(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: Rect {
                x: 0,
                y,
                width: 1920,
                height,
            },
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    #[test]
    fn reports_whether_outputs_actually_changed() {
        let snapshot = Snapshot::new();
        assert!(snapshot.set_outputs(vec![output("HDMI-A-1", 0, 1080)]));
        assert!(!snapshot.set_outputs(vec![output("HDMI-A-1", 0, 1080)]));
        assert!(snapshot.set_outputs(vec![]));
    }

    #[test]
    fn finds_an_output_by_name() {
        let snapshot = Snapshot::new();
        snapshot.set_outputs(vec![output("HDMI-A-1", 0, 1080)]);
        assert!(snapshot.output("HDMI-A-1").is_some());
        assert!(snapshot.output("HDMI-A-9").is_none());
    }

    #[test]
    fn layout_height_covers_stacked_outputs() {
        let snapshot = Snapshot::new();
        snapshot.set_outputs(vec![output("a", 0, 1080), output("b", 1080, 2160)]);
        assert_eq!(snapshot.layout_height(), 3240);
    }

    #[test]
    fn layout_height_ignores_inactive_outputs() {
        let snapshot = Snapshot::new();
        let mut inactive = output("b", 1080, 2160);
        inactive.active = false;
        snapshot.set_outputs(vec![output("a", 0, 1080), inactive]);
        assert_eq!(snapshot.layout_height(), 1080);
    }

    #[test]
    fn status_change_detection() {
        let snapshot = Snapshot::new();
        assert!(!snapshot.set_status(Status::default()));
        assert!(snapshot.set_status(Status {
            state: SyncState::Degraded,
            ..Default::default()
        }));
    }

    #[test]
    fn projection_stats_change_detection() {
        use crate::model::{CaptureIntervals, FrameCost, ProjectionStats};

        let snapshot = Snapshot::new();
        assert!(snapshot.projection_stats().is_none());
        assert!(
            !snapshot.set_projection_stats(None),
            "None to None is no change"
        );

        let stats = ProjectionStats {
            measured_at: 1,
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
            renderer: "cpu".to_string(),
            capture_intervals: CaptureIntervals::default(),
            outputs: Vec::new(),
        };
        assert!(snapshot.set_projection_stats(Some(stats.clone())));
        assert!(
            !snapshot.set_projection_stats(Some(stats)),
            "identical stats are no change"
        );
        assert!(
            snapshot.set_projection_stats(None),
            "clearing after running is a change"
        );
    }

    #[test]
    fn slicer_running_round_trips_and_reports_change() {
        let snapshot = Snapshot::new();
        assert!(!snapshot.slicer_running(), "nothing has run yet");
        assert!(
            !snapshot.set_slicer_running(false),
            "false to false is no change"
        );

        assert!(snapshot.set_slicer_running(true));
        assert!(snapshot.slicer_running());
        assert!(
            !snapshot.set_slicer_running(true),
            "true to true is no change"
        );

        assert!(snapshot.set_slicer_running(false));
        assert!(!snapshot.slicer_running());
    }

    #[test]
    fn projection_report_combines_liveness_and_the_last_interval() {
        use crate::model::{CaptureIntervals, FrameCost, ProjectionStats};

        let snapshot = Snapshot::new();
        let report = snapshot.projection_report();
        assert!(!report.running);
        assert!(report.last_interval.is_none());

        // Running, but nothing captured yet: the 2026-09-15 case this field
        // exists for — a static page produces no frames, so there is still
        // no interval to report even though the slicer is alive.
        snapshot.set_slicer_running(true);
        let report = snapshot.projection_report();
        assert!(report.running);
        assert!(report.last_interval.is_none());

        let stats = ProjectionStats {
            measured_at: 1,
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
            renderer: "cpu".to_string(),
            capture_intervals: CaptureIntervals::default(),
            outputs: Vec::new(),
        };
        snapshot.set_projection_stats(Some(stats.clone()));
        let report = snapshot.projection_report();
        assert!(report.running);
        assert_eq!(report.last_interval, Some(stats));
    }
}
