//! In-memory Sway client used by tests and by `suede run --mock`.
//!
//! Records every command it is given so tests can assert on the exact
//! reconciliation plan that reached the compositor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::raw::{collect_windows, RawNode, RawOutput};
use super::{SwayClient, SwayError, SwayEvent, SwayResult, SwayVersion};
use crate::model::{Mode, Output, Window};

pub struct MockSway {
    outputs: Mutex<Vec<Output>>,
    windows: Mutex<Vec<Window>>,
    commands: Mutex<Vec<String>>,
    /// Commands containing any of these substrings fail, to exercise error paths.
    failing: Mutex<Vec<String>>,
    version: Mutex<SwayVersion>,
    events: broadcast::Sender<SwayEvent>,
    connected: AtomicBool,
}

impl Default for MockSway {
    fn default() -> Self {
        Self::empty()
    }
}

impl MockSway {
    pub fn empty() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            outputs: Mutex::new(Vec::new()),
            windows: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
            failing: Mutex::new(Vec::new()),
            version: Mutex::new(SwayVersion {
                major: 1,
                minor: 10,
                patch: 0,
                human_readable: Some("1.10 (mock)".into()),
            }),
            events,
            connected: AtomicBool::new(true),
        }
    }

    /// Populated from the recorded fixtures: three outputs, two windows.
    pub fn with_fixtures() -> Self {
        let mock = Self::empty();
        let raw: Vec<RawOutput> =
            serde_json::from_str(include_str!("fixtures/get_outputs.json")).expect("valid fixture");
        *mock.outputs.lock().unwrap() = raw.into_iter().map(Output::from).collect();
        let root: RawNode =
            serde_json::from_str(include_str!("fixtures/get_tree.json")).expect("valid fixture");
        *mock.windows.lock().unwrap() = collect_windows(&root);
        mock
    }

    pub fn set_outputs(&self, outputs: Vec<Output>) {
        *self.outputs.lock().unwrap() = outputs;
        let _ = self.events.send(SwayEvent::OutputsMayHaveChanged);
    }

    pub fn set_windows(&self, windows: Vec<Window>) {
        *self.windows.lock().unwrap() = windows;
    }

    pub fn push_window(&self, window: Window) {
        let change = "new".to_string();
        self.windows.lock().unwrap().push(window.clone());
        let _ = self.events.send(SwayEvent::Window { change, window });
    }

    pub fn set_version(&self, version: SwayVersion) {
        *self.version.lock().unwrap() = version;
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Make any command containing `substring` fail.
    pub fn fail_commands_containing(&self, substring: impl Into<String>) {
        self.failing.lock().unwrap().push(substring.into());
    }

    /// Every command received, in order.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    pub fn clear_commands(&self) {
        self.commands.lock().unwrap().clear();
    }

    /// Whether any recorded command contains `substring`.
    pub fn ran_command_containing(&self, substring: &str) -> bool {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.contains(substring))
    }

    pub fn emit(&self, event: SwayEvent) {
        let _ = self.events.send(event);
    }
}

#[async_trait]
impl SwayClient for MockSway {
    async fn get_outputs(&self) -> SwayResult<Vec<Output>> {
        Ok(self.outputs.lock().unwrap().clone())
    }

    async fn get_windows(&self) -> SwayResult<Vec<Window>> {
        Ok(self.windows.lock().unwrap().clone())
    }

    async fn run_command(&self, command: &str) -> SwayResult<()> {
        self.commands.lock().unwrap().push(command.to_string());
        {
            let failing = self.failing.lock().unwrap();
            if let Some(pattern) = failing.iter().find(|p| command.contains(p.as_str())) {
                return Err(SwayError::CommandFailed {
                    command: command.to_string(),
                    error: format!("mock failure for {pattern:?}"),
                });
            }
        }

        // Apply output commands to the simulated state. Without this a
        // reconciliation pass would never converge, because the outputs it
        // configures would stay stubbornly unconfigured.
        let mut outputs = self.outputs.lock().unwrap();
        if apply_output_command(&mut outputs, command) {
            drop(outputs);
            let _ = self.events.send(SwayEvent::OutputsMayHaveChanged);
        }
        Ok(())
    }

    async fn get_version(&self) -> SwayResult<SwayVersion> {
        Ok(self.version.lock().unwrap().clone())
    }

    fn subscribe(&self) -> broadcast::Receiver<SwayEvent> {
        self.events.subscribe()
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Apply `output <name> <setting> …` to simulated state.
///
/// Returns whether anything changed.
fn apply_output_command(outputs: &mut [Output], command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let ["output", name, setting, rest @ ..] = words.as_slice() else {
        return false;
    };
    let Some(output) = outputs.iter_mut().find(|output| output.name == *name) else {
        return false;
    };

    match (*setting, rest) {
        ("enable", _) => {
            if output.active {
                return false;
            }
            output.active = true;
            // Sway picks the preferred mode when an output comes up.
            if output.current_mode.is_none() {
                output.current_mode = output.maximum_mode();
            }
            if output.modes.is_empty() {
                output.modes = vec![Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60.0,
                }];
                output.current_mode = output.modes.first().copied();
            }
            if let Some(mode) = output.current_mode {
                output.rect.width = mode.width;
                output.rect.height = mode.height;
            }
            true
        }
        ("disable", _) => {
            if !output.active {
                return false;
            }
            output.active = false;
            output.current_mode = None;
            output.rect = Default::default();
            true
        }
        ("mode", [spec, ..]) => match parse_mode(spec) {
            Some(mode) => {
                output.current_mode = Some(mode);
                output.rect.width = mode.width;
                output.rect.height = mode.height;
                if !output.modes.iter().any(|m| m.matches(&mode)) {
                    output.modes.push(mode);
                }
                true
            }
            None => false,
        },
        ("pos", [x, y, ..]) => match (x.parse::<i32>(), y.parse::<i32>()) {
            (Ok(x), Ok(y)) => {
                output.rect.x = x;
                output.rect.y = y;
                true
            }
            _ => false,
        },
        ("scale", [value, ..]) => match value.parse::<f64>() {
            Ok(scale) => {
                output.scale = Some(scale);
                true
            }
            Err(_) => false,
        },
        ("transform", [value, ..]) => {
            output.transform = Some((*value).to_string());
            true
        }
        ("adaptive_sync", [value, ..]) => {
            output.adaptive_sync_status = Some(
                if *value == "on" {
                    "enabled"
                } else {
                    "disabled"
                }
                .to_string(),
            );
            true
        }
        // `tearing` and `max_render_time` are accepted but not reported back by
        // sway, so the mock does not model them either.
        _ => false,
    }
}

/// Parse `1920x1080@60Hz` as sway formats it.
fn parse_mode(spec: &str) -> Option<Mode> {
    let (size, refresh) = match spec.split_once('@') {
        Some((size, refresh)) => (size, refresh.trim_end_matches("Hz").parse().ok()?),
        None => (spec, 60.0),
    };
    let (width, height) = size.split_once('x')?;
    Some(Mode {
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        refresh_hz: refresh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enabling_an_output_updates_simulated_state() {
        let mock = MockSway::with_fixtures();
        assert!(!mock.get_outputs().await.unwrap()[2].active);

        mock.run_command("output HDMI-A-3 enable").await.unwrap();
        let outputs = mock.get_outputs().await.unwrap();
        assert!(outputs[2].active);
        assert!(outputs[2].current_mode.is_some());
    }

    #[tokio::test]
    async fn disabling_an_output_updates_simulated_state() {
        let mock = MockSway::with_fixtures();
        mock.run_command("output HDMI-A-1 disable").await.unwrap();
        assert!(!mock.get_outputs().await.unwrap()[0].active);
    }

    #[tokio::test]
    async fn mode_and_position_are_applied() {
        let mock = MockSway::with_fixtures();
        mock.run_command("output HDMI-A-1 mode 1280x720@60Hz")
            .await
            .unwrap();
        mock.run_command("output HDMI-A-1 pos 100 200")
            .await
            .unwrap();

        let output = &mock.get_outputs().await.unwrap()[0];
        assert_eq!(output.current_mode.unwrap().width, 1280);
        assert_eq!((output.rect.x, output.rect.y), (100, 200));
    }

    #[tokio::test]
    async fn adaptive_sync_is_applied() {
        let mock = MockSway::with_fixtures();
        mock.run_command("output HDMI-A-1 adaptive_sync on")
            .await
            .unwrap();
        assert_eq!(
            mock.get_outputs().await.unwrap()[0]
                .adaptive_sync_status
                .as_deref(),
            Some("enabled")
        );
    }

    #[tokio::test]
    async fn unrelated_commands_change_nothing() {
        let mock = MockSway::with_fixtures();
        let before = mock.get_outputs().await.unwrap();
        mock.run_command("[con_id=1] fullscreen enable")
            .await
            .unwrap();
        mock.run_command("seat seat0 hide_cursor 1000")
            .await
            .unwrap();
        assert_eq!(mock.get_outputs().await.unwrap(), before);
    }

    #[test]
    fn mode_parsing_matches_sway_formatting() {
        assert_eq!(
            parse_mode("1920x1080@60Hz"),
            Some(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0
            })
        );
        assert_eq!(parse_mode("3840x2160@59.997Hz").unwrap().refresh_hz, 59.997);
        assert!(parse_mode("nonsense").is_none());
    }

    #[tokio::test]
    async fn fixtures_load() {
        let mock = MockSway::with_fixtures();
        assert_eq!(mock.get_outputs().await.unwrap().len(), 3);
        assert_eq!(mock.get_windows().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn records_commands() {
        let mock = MockSway::empty();
        mock.run_command("output HDMI-A-1 enable").await.unwrap();
        assert_eq!(mock.commands(), vec!["output HDMI-A-1 enable"]);
        assert!(mock.ran_command_containing("enable"));
    }

    #[tokio::test]
    async fn simulates_failures() {
        let mock = MockSway::empty();
        mock.fail_commands_containing("mode");
        assert!(mock.run_command("output HDMI-A-1 enable").await.is_ok());
        assert!(mock
            .run_command("output HDMI-A-1 mode 1920x1080@60Hz")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn broadcasts_events() {
        let mock = MockSway::empty();
        let mut receiver = mock.subscribe();
        mock.set_outputs(Vec::new());
        assert!(matches!(
            receiver.recv().await.unwrap(),
            SwayEvent::OutputsMayHaveChanged
        ));
    }
}
