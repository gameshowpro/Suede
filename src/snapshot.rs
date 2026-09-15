//! The live view of the world, shared between the reconciler and the API.
//!
//! Everything here is re-derived from Sway and PipeWire; none of it is persisted.

use std::sync::RwLock;

use crate::model::{Output, ProjectionStats, Status, Window};

#[derive(Default)]
pub struct Snapshot {
    outputs: RwLock<Vec<Output>>,
    windows: RwLock<Vec<Window>>,
    status: RwLock<Status>,
    /// What the slicer last reported, if one is running. Set by the manager's
    /// reader thread from the slicer's stdout; cleared when the slicer stops
    /// or is found to have exited (see `BlendManager`).
    projection_stats: RwLock<Option<ProjectionStats>>,
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
        use crate::model::{FrameCost, ProjectionStats};

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
            },
            presentation_feedback: true,
            offset_ms: None,
            straddles: 0,
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
}
