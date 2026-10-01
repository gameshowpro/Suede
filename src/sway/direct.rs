//! The compositor as the daemon sees it during direct presentation.
//!
//! In direct mode Sway is headless-only: it holds the canvas the active app
//! renders into and nothing else, while the slicer drives the physical
//! displays itself through `VK_KHR_display`. The rest of the daemon — the
//! reconciler and its output planner, adoption, `/outputs`, the UI, the
//! checks — is written against a compositor that reports and configures
//! those displays. [`DirectOutputs`] keeps all of that working unchanged by
//! standing in for Sway on the physical outputs only:
//!
//! - `get_outputs` returns the real (canvas) outputs plus one simulated
//!   output per connected display in the live
//!   [`DrmInventory`](crate::drm_inventory::DrmInventory);
//! - `output <physical> …` commands are applied to that simulated state with
//!   the mock compositor's own rules, and never reach Sway (which has no
//!   such output); everything else is forwarded to the real client as one
//!   batch, so the canvas and windows behave exactly as before.
//!
//! What a physical output can be told is what direct presentation can
//! honor: enable, disable, position, and a mode the display advertises.
//! Anything else that would change the picture — a scale other than 1, a
//! transform, adaptive sync, a custom mode — is refused as a failed command,
//! which the reconciler reports as a `command_failed` divergence. Background,
//! tearing and render-time settings have no meaning without a compositor on
//! the output and are accepted as no-ops, so an ordinary configuration does
//! not diverge for them.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::mock::{apply_output_command, parse_mode};
use super::{SwayClient, SwayError, SwayEvent, SwayResult, SwayVersion};
use crate::drm_inventory::DrmInventory;
use crate::model::{Mode, Output, Window};

/// A [`SwayClient`] that owns the physical outputs and forwards the rest.
pub struct DirectOutputs {
    inner: Arc<dyn SwayClient>,
    /// Simulated state of every connected display, in inventory order.
    owned: Mutex<Vec<Output>>,
    /// The inventory's preferred mode per owned output, which Sway would
    /// pick when an output is enabled with no mode configured.
    preferred: Vec<(String, Option<Mode>)>,
    events: broadcast::Sender<SwayEvent>,
}

impl DirectOutputs {
    /// Wrap `inner`, taking ownership of every connected display in
    /// `inventory`. Starts from the state a DRM session would come up in:
    /// every display enabled at its preferred mode, tiled left to right.
    ///
    /// Must be called inside a Tokio runtime: the inner client's events are
    /// forwarded onto this client's own channel by a spawned task, so
    /// subscribers see both real and simulated changes on one receiver.
    pub fn new(inner: Arc<dyn SwayClient>, inventory: &DrmInventory) -> Arc<Self> {
        let owned = inventory.simulated_outputs();
        let preferred = inventory
            .connected()
            .map(|output| {
                (
                    output.name.clone(),
                    output.preferred_mode().map(|mode| Mode {
                        width: mode.width,
                        height: mode.height,
                        refresh_hz: f64::from(mode.refresh_millihz) / 1000.0,
                    }),
                )
            })
            .collect();
        let (events, _) = broadcast::channel(64);
        let client = Arc::new(Self {
            inner: inner.clone(),
            owned: Mutex::new(owned),
            preferred,
            events: events.clone(),
        });
        let mut upstream = inner.subscribe();
        tokio::spawn(async move {
            loop {
                match upstream.recv().await {
                    Ok(event) => {
                        let _ = events.send(event);
                    }
                    // Whatever was missed may have included an output
                    // change; a re-query is always a safe answer.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = events.send(SwayEvent::OutputsMayHaveChanged);
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        client
    }

    /// Whether `name` is a physical output this client simulates.
    pub fn owns(&self, name: &str) -> bool {
        self.preferred.iter().any(|(owned, _)| owned == name)
    }

    /// The owned output a command addresses, if it is an `output <owned> …`
    /// command at all.
    fn owned_target<'a>(&self, command: &'a str) -> Option<&'a str> {
        let mut words = command.split_whitespace();
        if words.next() != Some("output") {
            return None;
        }
        words.next().filter(|name| self.owns(name))
    }

    /// Vet and apply one `output <owned> …` command to the simulated state.
    /// Returns whether the simulated state changed.
    fn apply_owned(&self, command: &str) -> SwayResult<bool> {
        let refuse = |reason: &str| {
            Err(SwayError::CommandFailed {
                command: command.to_string(),
                error: format!("{reason} (experimental direct presentation)"),
            })
        };
        let words: Vec<&str> = command.split_whitespace().collect();
        let ["output", name, setting, rest @ ..] = words.as_slice() else {
            return refuse("incomplete output command");
        };
        let mut outputs = self.owned.lock().unwrap();
        let Some(output) = outputs.iter_mut().find(|output| output.name == *name) else {
            return refuse("unknown output");
        };

        let normalized = match (*setting, rest) {
            ("enable", _) => {
                // Sway brings an output up at its preferred mode; the shared
                // simulation would otherwise pick the largest one.
                if !output.active && output.current_mode.is_none() {
                    output.current_mode = self
                        .preferred
                        .iter()
                        .find(|(owned, _)| owned == name)
                        .and_then(|(_, mode)| *mode);
                }
                command.to_string()
            }
            ("disable", _) => command.to_string(),
            ("pos" | "position", [x, y, ..]) => format!("output {name} pos {x} {y}"),
            ("mode" | "resolution" | "res", [first, ..]) => {
                if *first == "--custom" {
                    return refuse("custom modes cannot be presented directly; choose a mode the display advertises");
                }
                let Some(wanted) = parse_mode(first) else {
                    return refuse("unreadable mode");
                };
                let advertised = if first.contains('@') {
                    output.modes.iter().find(|mode| mode.matches(&wanted))
                } else {
                    // No rate given: the fastest the display offers at that size.
                    output
                        .modes
                        .iter()
                        .filter(|mode| mode.width == wanted.width && mode.height == wanted.height)
                        .max_by(|a, b| a.refresh_hz.total_cmp(&b.refresh_hz))
                };
                let Some(advertised) = advertised else {
                    return refuse(&format!(
                        "{name} does not advertise {}; direct presentation only uses advertised modes",
                        wanted.to_sway()
                    ));
                };
                format!("output {name} mode {}", advertised.to_sway())
            }
            ("scale", [value, ..]) => match value.parse::<f64>() {
                Ok(scale) if (scale - 1.0).abs() < 1e-9 => command.to_string(),
                _ => return refuse("only scale 1 is supported for a directly presented output"),
            },
            ("transform", [value]) if *value == "normal" => command.to_string(),
            ("transform", _) => {
                return refuse("transforms are not supported for a directly presented output")
            }
            ("adaptive_sync", [value, ..])
                if matches!(*value, "off" | "disable" | "disabled" | "no" | "false") =>
            {
                command.to_string()
            }
            ("adaptive_sync", _) => {
                return refuse("adaptive sync is not supported for a directly presented output")
            }
            // Nothing composites onto a directly presented output, so these
            // have nothing to apply to.
            ("bg" | "background" | "allow_tearing" | "tearing" | "max_render_time", _) => {
                return Ok(false)
            }
            _ => return refuse("not supported for a directly presented output"),
        };
        Ok(apply_output_command(&mut outputs, &normalized))
    }

    fn announce(&self) {
        let _ = self.events.send(SwayEvent::OutputsMayHaveChanged);
    }
}

#[async_trait]
impl SwayClient for DirectOutputs {
    async fn get_outputs(&self) -> SwayResult<Vec<Output>> {
        let mut outputs: Vec<Output> = self
            .inner
            .get_outputs()
            .await?
            .into_iter()
            // The simulated output is the authority on a physical name; a
            // compositor that also reports one is not one this mode expects.
            .filter(|output| !self.owns(&output.name))
            .collect();
        outputs.extend(self.owned.lock().unwrap().iter().cloned());
        Ok(outputs)
    }

    async fn get_windows(&self) -> SwayResult<Vec<Window>> {
        self.inner.get_windows().await
    }

    async fn run_command(&self, command: &str) -> SwayResult<()> {
        if self.owned_target(command).is_none() {
            return self.inner.run_command(command).await;
        }
        let changed = self.apply_owned(command)?;
        if changed {
            self.announce();
        }
        Ok(())
    }

    /// Owned commands are applied here, in order; the rest go to the inner
    /// client as one batch (one Sway IPC message, one backend commit), and
    /// every result lands back in its caller's slot.
    async fn run_commands(&self, commands: &[String]) -> Vec<SwayResult<()>> {
        let mut results: Vec<Option<SwayResult<()>>> = Vec::with_capacity(commands.len());
        let mut forwarded = Vec::new();
        let mut forwarded_slots = Vec::new();
        let mut changed = false;
        for (slot, command) in commands.iter().enumerate() {
            if self.owned_target(command).is_some() {
                results.push(Some(self.apply_owned(command).map(|applied| {
                    changed |= applied;
                })));
            } else {
                results.push(None);
                forwarded.push(command.clone());
                forwarded_slots.push(slot);
            }
        }
        if !forwarded.is_empty() {
            let mut inner = self.inner.run_commands(&forwarded).await.into_iter();
            for slot in forwarded_slots {
                results[slot] = Some(inner.next().unwrap_or_else(|| {
                    Err(SwayError::CommandFailed {
                        command: commands[slot].clone(),
                        error: "the compositor returned no result for this command".to_string(),
                    })
                }));
            }
        }
        if changed {
            self.announce();
        }
        results
            .into_iter()
            .map(|result| result.expect("every slot is filled above"))
            .collect()
    }

    async fn get_version(&self) -> SwayResult<SwayVersion> {
        self.inner.get_version().await
    }

    fn subscribe(&self) -> broadcast::Receiver<SwayEvent> {
        self.events.subscribe()
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_inventory::test_support::wall;
    use crate::model::Rect;
    use crate::sway::mock::MockSway;

    /// A headless-only compositor: just the canvas.
    fn headless() -> Arc<MockSway> {
        let mock = MockSway::empty();
        mock.set_outputs(vec![Output {
            name: "HEADLESS-1".into(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: Some(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0,
            }),
            modes: Vec::new(),
            rect: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale: Some(1.0),
            transform: Some("normal".into()),
            adaptive_sync_status: Some("disabled".into()),
        }]);
        Arc::new(mock)
    }

    fn client(inner: &Arc<MockSway>) -> Arc<DirectOutputs> {
        DirectOutputs::new(inner.clone(), &wall(&["DP-1", "DP-2"]))
    }

    async fn output(client: &DirectOutputs, name: &str) -> Output {
        client
            .get_outputs()
            .await
            .unwrap()
            .into_iter()
            .find(|output| output.name == name)
            .unwrap()
    }

    #[tokio::test]
    async fn reports_the_canvas_plus_every_connected_display() {
        let inner = headless();
        let client = client(&inner);
        let outputs = client.get_outputs().await.unwrap();
        let names: Vec<&str> = outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["HEADLESS-1", "DP-1", "DP-2"]);

        let dp2 = &outputs[2];
        assert!(dp2.active);
        assert_eq!(dp2.make.as_deref(), Some("Acme"));
        assert_eq!(dp2.model.as_deref(), Some("DP-2"));
        // Preferred mode, tiled after DP-1.
        assert_eq!(dp2.current_mode.unwrap().refresh_hz, 60.0);
        assert_eq!(dp2.rect.x, 1920);
        assert_eq!(dp2.modes.len(), 2);
    }

    #[tokio::test]
    async fn a_compositor_reporting_a_physical_name_is_overridden() {
        let inner = headless();
        let mut outputs = inner.get_outputs().await.unwrap();
        let mut real = outputs[0].clone();
        real.name = "DP-1".into();
        real.make = Some("Real".into());
        outputs.push(real);
        inner.set_outputs(outputs);
        let client = client(&inner);
        let reported = client.get_outputs().await.unwrap();
        let dp1: Vec<&Output> = reported.iter().filter(|o| o.name == "DP-1").collect();
        assert_eq!(dp1.len(), 1);
        assert_eq!(dp1[0].make.as_deref(), Some("Acme"));
    }

    #[tokio::test]
    async fn owned_commands_are_simulated_and_the_rest_forwarded_in_one_batch() {
        let inner = headless();
        let client = client(&inner);
        let commands: Vec<String> = [
            "output DP-1 disable",
            "output HEADLESS-1 mode --custom 3840x1080@59.939Hz",
            "output DP-2 pos 100 200",
            "output DP-2 mode 1920x1080@59.939Hz",
            "[con_id=7] fullscreen enable",
        ]
        .iter()
        .map(|c| c.to_string())
        .collect();

        let results = client.run_commands(&commands).await;
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(
            inner.commands(),
            [
                "output HEADLESS-1 mode --custom 3840x1080@59.939Hz",
                "[con_id=7] fullscreen enable"
            ],
            "only non-physical commands reach sway, in order"
        );

        assert!(!output(&client, "DP-1").await.active);
        let dp2 = output(&client, "DP-2").await;
        assert_eq!((dp2.rect.x, dp2.rect.y), (100, 200));
        assert_eq!(dp2.current_mode.unwrap().refresh_hz, 59.939);
        // The canvas change went through the real (mock) compositor.
        assert_eq!(output(&client, "HEADLESS-1").await.rect.width, 3840);
    }

    #[tokio::test]
    async fn inner_failures_land_in_their_own_slots() {
        let inner = headless();
        inner.fail_commands_containing("bogus");
        let client = client(&inner);
        let commands: Vec<String> = [
            "output HEADLESS-1 bogus",
            "output DP-1 scale 2",
            "output DP-2 pos 0 0",
            "seat seat0 hide_cursor 1000",
        ]
        .iter()
        .map(|c| c.to_string())
        .collect();
        let results = client.run_commands(&commands).await;
        assert!(results[0].is_err(), "the compositor's own failure");
        assert!(results[1].is_err(), "a refused physical setting");
        assert!(results[2].is_ok());
        assert!(results[3].is_ok());
    }

    #[tokio::test]
    async fn settings_direct_presentation_cannot_honor_are_refused() {
        let inner = headless();
        let client = client(&inner);
        let before = output(&client, "DP-1").await;
        for command in [
            "output DP-1 scale 2",
            "output DP-1 scale 1.5",
            "output DP-1 transform 90",
            "output DP-1 transform flipped-180",
            "output DP-1 adaptive_sync on",
            "output DP-1 mode --custom 1920x1080@60Hz",
            "output DP-1 mode 1280x720@60Hz",
            "output DP-1 mode 1920x1080@50Hz",
            "output DP-1 power off",
            "output DP-1 unplug",
        ] {
            let error = client
                .run_command(command)
                .await
                .expect_err(command)
                .to_string();
            assert!(error.contains("direct presentation"), "{command}: {error}");
        }
        assert_eq!(output(&client, "DP-1").await, before, "nothing applied");
        assert!(inner.commands().is_empty(), "nothing forwarded");
    }

    #[tokio::test]
    async fn neutral_settings_and_compositor_only_settings_are_accepted() {
        let inner = headless();
        let client = client(&inner);
        for command in [
            "output DP-1 scale 1",
            "output DP-1 transform normal",
            "output DP-1 adaptive_sync off",
            "output DP-1 allow_tearing no",
            "output DP-1 max_render_time off",
            "output DP-1 bg #223344 solid_color",
            "output DP-1 mode 1920x1080",
        ] {
            client.run_command(command).await.expect(command);
        }
        assert!(inner.commands().is_empty(), "nothing forwarded");
        // `mode WxH` with no rate takes the fastest advertised.
        let mode = output(&client, "DP-1").await.current_mode.unwrap();
        assert_eq!(mode.refresh_hz, 60.0);
    }

    #[tokio::test]
    async fn re_enabling_comes_back_at_the_preferred_mode() {
        let inner = headless();
        let client = client(&inner);
        client
            .run_command("output DP-1 mode 1920x1080@59.939Hz")
            .await
            .unwrap();
        client.run_command("output DP-1 disable").await.unwrap();
        let disabled = output(&client, "DP-1").await;
        assert!(!disabled.active && disabled.current_mode.is_none());

        client.run_command("output DP-1 enable").await.unwrap();
        let enabled = output(&client, "DP-1").await;
        assert!(enabled.active);
        assert_eq!(enabled.current_mode.unwrap().refresh_hz, 60.0);
        assert_eq!(enabled.rect.width, 1920);
    }

    #[tokio::test]
    async fn forwards_everything_that_is_not_a_physical_output() {
        let inner = headless();
        let client = client(&inner);
        client.run_command("create_output").await.unwrap();
        client
            .run_command("output HEADLESS-1 pos 0 20000")
            .await
            .unwrap();
        assert_eq!(
            inner.commands(),
            ["create_output", "output HEADLESS-1 pos 0 20000"]
        );
        assert_eq!(client.get_version().await.unwrap().minor, 10);
        assert!(client.is_connected());
        inner.set_connected(false);
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn simulated_and_real_changes_reach_one_subscriber() {
        let inner = headless();
        let client = client(&inner);
        let mut events = client.subscribe();

        client.run_command("output DP-1 disable").await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            SwayEvent::OutputsMayHaveChanged
        ));

        // A no-op on simulated state announces nothing.
        client
            .run_command("output DP-1 bg #000000 solid_color")
            .await
            .unwrap();
        inner.emit(SwayEvent::Shutdown);
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("the inner event is forwarded")
            .unwrap();
        assert!(matches!(next, SwayEvent::Shutdown), "{next:?}");
    }
}
