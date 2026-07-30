//! Sway's own JSON shapes, and the mapping into Suede's model types.
//!
//! Kept separate from [`crate::model`] so the public API is not hostage to
//! Sway's wire format.

use serde::Deserialize;

use crate::model::{Mode, Output, Rect, Window};

#[derive(Debug, Clone, Deserialize)]
pub struct RawRect {
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub width: i32,
    #[serde(default)]
    pub height: i32,
}

impl From<&RawRect> for Rect {
    fn from(value: &RawRect) -> Self {
        Rect {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RawMode {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in millihertz.
    #[serde(default)]
    pub refresh: i32,
}

impl From<RawMode> for Mode {
    fn from(value: RawMode) -> Self {
        Mode {
            width: value.width,
            height: value.height,
            refresh_hz: value.refresh as f64 / 1000.0,
        }
    }
}

/// One entry of `get_outputs`.
#[derive(Debug, Clone, Deserialize)]
pub struct RawOutput {
    pub name: String,
    #[serde(default)]
    pub active: bool,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub current_mode: Option<RawMode>,
    #[serde(default)]
    pub modes: Vec<RawMode>,
    pub rect: Option<RawRect>,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub adaptive_sync_status: Option<String>,
}

/// EDID fields come back as the literal string "Unknown" when absent.
fn clean(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty() && v != "Unknown")
}

impl From<RawOutput> for Output {
    fn from(value: RawOutput) -> Self {
        let mut modes: Vec<Mode> = Vec::with_capacity(value.modes.len());
        // Sway reports duplicates that differ only in fields it does not expose
        // (colour depth, interlacing), so collapse them.
        for mode in value.modes.into_iter().map(Mode::from) {
            if !modes.iter().any(|existing| existing.matches(&mode)) {
                modes.push(mode);
            }
        }
        modes.sort_by(|a, b| {
            (a.width, a.height).cmp(&(b.width, b.height)).then(
                a.refresh_hz
                    .partial_cmp(&b.refresh_hz)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });

        Output {
            name: value.name,
            active: value.active,
            make: clean(value.make),
            model: clean(value.model),
            serial: clean(value.serial),
            current_mode: value.current_mode.map(Mode::from),
            modes,
            rect: value.rect.as_ref().map(Rect::from).unwrap_or_default(),
            scale: value.scale,
            transform: value.transform,
            adaptive_sync_status: value.adaptive_sync_status,
        }
    }
}

/// A node of `get_tree`, and the container payload of window events.
#[derive(Debug, Clone, Deserialize)]
pub struct RawNode {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "type")]
    pub node_type: Option<String>,
    pub name: Option<String>,
    pub app_id: Option<String>,
    pub pid: Option<i32>,
    pub visible: Option<bool>,
    #[serde(default)]
    pub fullscreen_mode: i32,
    pub rect: Option<RawRect>,
    /// Present on X11 windows surfaced through XWayland.
    pub window: Option<i64>,
    #[serde(default)]
    pub nodes: Vec<RawNode>,
    #[serde(default)]
    pub floating_nodes: Vec<RawNode>,
}

impl RawNode {
    fn is_container(&self) -> bool {
        matches!(
            self.node_type.as_deref(),
            Some("con") | Some("floating_con")
        )
    }

    fn is_leaf(&self) -> bool {
        self.nodes.is_empty() && self.floating_nodes.is_empty()
    }

    /// A real window: a leaf container that actually has a surface.
    pub fn is_window(&self) -> bool {
        self.is_container() && self.is_leaf() && (self.app_id.is_some() || self.window.is_some())
    }

    pub fn to_window(&self, output: Option<&str>) -> Window {
        Window {
            id: self.id,
            title: self.name.clone(),
            app_id: self.app_id.clone(),
            pid: self.pid,
            visible: self.visible,
            fullscreen_mode: self.fullscreen_mode,
            rect: self.rect.as_ref().map(Rect::from).unwrap_or_default(),
            output: output.map(str::to_string),
            app: None,
        }
    }
}

/// Walk a tree, collecting real windows and remembering which output they sit under.
pub fn collect_windows(root: &RawNode) -> Vec<Window> {
    let mut windows = Vec::new();
    walk(root, None, &mut windows);
    windows
}

fn walk(node: &RawNode, output: Option<&str>, windows: &mut Vec<Window>) {
    // `__i3` is the scratchpad's pseudo-output; windows parked there have no real output.
    let current_output = if node.node_type.as_deref() == Some("output") {
        node.name.as_deref().filter(|name| *name != "__i3")
    } else {
        output
    };

    if node.is_window() {
        windows.push(node.to_window(current_output));
        return;
    }

    for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
        walk(child, current_output, windows);
    }
}

/// Reply to `run_command`, one entry per command in the payload.
#[derive(Debug, Clone, Deserialize)]
pub struct RawCommandOutcome {
    #[serde(default)]
    pub success: bool,
    pub error: Option<String>,
}

/// Reply to `get_version`.
#[derive(Debug, Clone, Deserialize)]
pub struct RawVersion {
    #[serde(default)]
    pub major: u32,
    #[serde(default)]
    pub minor: u32,
    #[serde(default)]
    pub patch: u32,
    pub human_readable: Option<String>,
}

/// Payload of a `window` event.
#[derive(Debug, Clone, Deserialize)]
pub struct RawWindowEvent {
    pub change: Option<String>,
    pub container: Option<RawNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUTS: &str = include_str!("fixtures/get_outputs.json");
    const TREE: &str = include_str!("fixtures/get_tree.json");
    const WINDOW_EVENT: &str = include_str!("fixtures/event_window_new.json");

    fn outputs() -> Vec<Output> {
        serde_json::from_str::<Vec<RawOutput>>(OUTPUTS)
            .unwrap()
            .into_iter()
            .map(Output::from)
            .collect()
    }

    #[test]
    fn parses_outputs_from_a_real_session() {
        let outputs = outputs();
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].name, "HDMI-A-1");
        assert!(outputs[0].active);
        assert_eq!(outputs[0].make.as_deref(), Some("Acme Displays"));
        assert_eq!(
            outputs[0].current_mode,
            Some(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0
            })
        );
    }

    #[test]
    fn converts_millihertz_to_hertz() {
        let outputs = outputs();
        let mode = outputs[1].current_mode.unwrap();
        assert!((mode.refresh_hz - 59.997).abs() < 1e-6);
    }

    #[test]
    fn deduplicates_modes() {
        let outputs = outputs();
        // The fixture lists 1920x1080@60 three times, as a real EDID often does.
        let sixties = outputs[0]
            .modes
            .iter()
            .filter(|m| m.width == 1920 && m.height == 1080 && (m.refresh_hz - 60.0).abs() < 0.01)
            .count();
        assert_eq!(sixties, 1);
    }

    #[test]
    fn treats_unknown_edid_strings_as_absent() {
        let outputs = outputs();
        assert_eq!(outputs[2].make, None);
        assert_eq!(outputs[2].serial, None);
    }

    #[test]
    fn inactive_output_is_reported() {
        let outputs = outputs();
        assert!(!outputs[2].active);
    }

    #[test]
    fn collects_windows_with_their_output() {
        let root: RawNode = serde_json::from_str(TREE).unwrap();
        let windows = collect_windows(&root);
        assert_eq!(windows.len(), 2, "expected two real windows: {windows:#?}");

        let first = windows.iter().find(|w| w.pid == Some(4242)).unwrap();
        assert_eq!(first.app_id.as_deref(), Some("chromium"));
        assert_eq!(first.output.as_deref(), Some("HDMI-A-1"));

        let second = windows.iter().find(|w| w.pid == Some(4343)).unwrap();
        assert_eq!(second.output.as_deref(), Some("HDMI-A-2"));
    }

    #[test]
    fn skips_split_containers_and_the_scratchpad_output() {
        let root: RawNode = serde_json::from_str(TREE).unwrap();
        let windows = collect_windows(&root);
        // The split container in the fixture must not be reported as a window.
        assert!(windows.iter().all(|w| w.app_id.is_some()));
        assert!(windows.iter().all(|w| w.output.as_deref() != Some("__i3")));
    }

    #[test]
    fn parses_a_window_event() {
        let event: RawWindowEvent = serde_json::from_str(WINDOW_EVENT).unwrap();
        assert_eq!(event.change.as_deref(), Some("new"));
        let container = event.container.unwrap();
        assert!(container.is_window());
        assert_eq!(container.pid, Some(4242));
    }
}
