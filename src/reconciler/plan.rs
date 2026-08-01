//! Pure planning: given observed and desired state, produce the exact commands
//! needed to close the gap.
//!
//! This module performs no IO, which is what makes the reconciliation rules
//! testable without a compositor. The executor in [`super`] runs the plan.

use std::collections::HashMap;

use crate::model::{AppConfig, Background, Divergence, Output, OutputConfig};

/// Version-gated compositor features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub supports_tearing: bool,
}

/// What Suede last applied to an output.
///
/// Needed for settings Sway does not report back in `get_outputs` (tearing and
/// max render time); everything else is diffed against observed state, which is
/// authoritative and survives a daemon restart.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedOutput {
    pub config: OutputConfig,
    pub workspace: u32,
    /// The background as *resolved*, not as referenced.
    ///
    /// Diffing the reference would miss an edit to a preset the output points
    /// at: the name is unchanged while the picture behind it is not.
    pub background: Option<Background>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutputPlan {
    /// Sway commands, in the order they must be issued.
    pub commands: Vec<String>,
    /// Desired state that could not be realized.
    pub divergences: Vec<Divergence>,
    /// True when an output was enabled or disabled, so the layout must settle.
    pub topology_changed: bool,
    /// Live output name → the workspace pinned to it.
    pub workspaces: HashMap<String, u32>,
    /// What Suede has now applied, to feed the next pass.
    pub applied: HashMap<String, AppliedOutput>,
}

/// Diff observed outputs against desired ones.
///
/// Outputs with no desired entry are deliberately left untouched: disabling one
/// requires an explicit entry with `enable: false`.
pub fn plan_outputs(
    observed: &[Output],
    desired: &[OutputConfig],
    previously_applied: &HashMap<String, AppliedOutput>,
    capabilities: Capabilities,
) -> OutputPlan {
    plan_outputs_with(
        observed,
        desired,
        &[],
        previously_applied,
        capabilities,
        |_| None,
    )
}

/// As [`plan_outputs`], with background presets and a resolver from wallpaper
/// id to file path.
///
/// The resolver is injected rather than reading from disk here, so the planner
/// stays pure and the background rules remain testable without any files
/// existing.
pub fn plan_outputs_with<F>(
    observed: &[Output],
    desired: &[OutputConfig],
    presets: &[crate::model::BackgroundPreset],
    previously_applied: &HashMap<String, AppliedOutput>,
    capabilities: Capabilities,
    resolve_wallpaper: F,
) -> OutputPlan
where
    F: Fn(&str) -> Option<String>,
{
    let mut plan = OutputPlan::default();

    for (index, config) in desired.iter().enumerate() {
        let workspace = index as u32 + 1;
        let key = config.r#match.key();

        let Some(output) = observed
            .iter()
            .find(|output| config.r#match.matches(output))
        else {
            plan.divergences.push(Divergence::new(
                "output_not_connected",
                &key,
                format!("no connected output matches {key}"),
            ));
            continue;
        };
        let name = output.name.as_str();

        if !config.enable {
            if output.active {
                plan.commands.push(format!("output {name} disable"));
                plan.topology_changed = true;
            }
            continue;
        }

        let applied = previously_applied.get(name);

        // Enabling an output resets everything Sway knows about it, so once we
        // issue `enable` every other setting must be re-applied unconditionally.
        let just_enabled = !output.active;
        if just_enabled {
            plan.commands.push(format!("output {name} enable"));
            plan.topology_changed = true;
        }

        // The first time Suede manages an output, apply every setting even if
        // the observed value already matches. An observed value that Suede did
        // not set is not necessarily pinned: sway auto-arranges outputs it has
        // no explicit position for, and will silently recompute that position
        // when a neighbouring output changes size. Applying once makes it
        // explicit; later passes go back to minimal diffs.
        let unmanaged = applied.is_none();
        let force = just_enabled || unmanaged;

        if let Some(mode) = config.mode {
            // An inactive output reports no modes, so the request is passed
            // through and sway validates it once the output comes up.
            let target = if output.modes.is_empty() {
                Some(mode)
            } else {
                output.resolve_mode(&mode)
            };

            match target {
                None => plan.divergences.push(Divergence::new(
                    "mode_unsupported",
                    name,
                    format!("{name} does not advertise mode {}", mode.to_sway()),
                )),
                Some(target) => {
                    if !target.matches(&mode) {
                        tracing::info!(
                            output = name,
                            requested = %mode.to_sway(),
                            using = %target.to_sway(),
                            "using the nearest advertised refresh rate"
                        );
                    }
                    // Issue the advertised mode rather than the requested one,
                    // so the next pass sees it as already satisfied.
                    if force
                        || output
                            .current_mode
                            .is_none_or(|current| !current.matches(&target))
                    {
                        plan.commands
                            .push(format!("output {name} mode {}", target.to_sway()));
                    }
                }
            }
        }

        if let Some(position) = config.position {
            if force || output.rect.x != position.x || output.rect.y != position.y {
                plan.commands
                    .push(format!("output {name} pos {} {}", position.x, position.y));
            }
        }

        if let Some(scale) = config.scale {
            let differs = output
                .scale
                .is_none_or(|current| (current - scale).abs() > 1e-6);
            if force || differs {
                plan.commands
                    .push(format!("output {name} scale {}", format_number(scale)));
            }
        }

        if let Some(transform) = config.transform {
            let wanted = transform.as_sway();
            if force || output.transform.as_deref() != Some(wanted) {
                plan.commands
                    .push(format!("output {name} transform {wanted}"));
            }
        }

        let adaptive_sync_active = output
            .adaptive_sync_status
            .as_deref()
            .map(|status| status == "enabled");
        if force || adaptive_sync_active != Some(config.adaptive_sync) {
            plan.commands.push(format!(
                "output {name} adaptive_sync {}",
                if config.adaptive_sync { "on" } else { "off" }
            ));
        }

        if capabilities.supports_tearing {
            let changed = applied.is_none_or(|a| a.config.allow_tearing != config.allow_tearing);
            if force || changed {
                // `allow_tearing` is the subcommand sway accepts; a plain
                // `tearing` is rejected as an invalid output subcommand.
                plan.commands.push(format!(
                    "output {name} allow_tearing {}",
                    if config.allow_tearing { "yes" } else { "no" }
                ));
            }
        } else if config.allow_tearing {
            plan.divergences.push(Divergence::new(
                "tearing_unsupported",
                name,
                "this sway version does not support tearing control (needs 1.10 or newer)",
            ));
        }

        let render_time_changed =
            applied.is_none_or(|a| a.config.max_render_time_ms != config.max_render_time_ms);
        if force || render_time_changed {
            let value = config
                .max_render_time_ms
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| "off".to_string());
            plan.commands
                .push(format!("output {name} max_render_time {value}"));
        }

        // Sway does not report the background in `get_outputs`, so like tearing
        // it is diffed against what Suede last applied — and against the
        // resolved properties, so editing a preset repaints every output using
        // it even though none of their references changed.
        let mut resolved_background = None;
        if let Some(reference) = &config.background {
            match reference.resolve(presets) {
                Some(background) => {
                    resolved_background = Some(background.clone());
                    let changed = applied.is_none_or(|a| a.background.as_ref() != Some(background));
                    if force || changed {
                        match background_command(name, background, &resolve_wallpaper) {
                            Ok(Some(command)) => plan.commands.push(command),
                            Ok(None) => {}
                            Err(divergence) => plan.divergences.push(divergence),
                        }
                    }
                }
                None => plan.divergences.push(Divergence::new(
                    "background_preset_not_found",
                    name,
                    format!(
                        "{name} refers to background preset {:?}, which is not defined",
                        reference.preset_id().unwrap_or_default()
                    ),
                )),
            }
        }

        // Pinning a workspace to the output makes app placement deterministic.
        if force || applied.is_none_or(|a| a.workspace != workspace) {
            plan.commands
                .push(format!("workspace {workspace} output {name}"));
        }

        plan.workspaces.insert(name.to_string(), workspace);
        plan.applied.insert(
            name.to_string(),
            AppliedOutput {
                config: config.clone(),
                background: resolved_background,
                workspace,
            },
        );
    }

    plan
}

/// Where a managed app should run.
#[derive(Debug, Clone, PartialEq)]
pub struct AppTarget {
    pub id: String,
    /// Live output the window should be placed on, when the app pins one.
    pub output: Option<String>,
    /// Workspace to move the window to, when an output is pinned.
    pub workspace: Option<u32>,
    /// Set when the app cannot run right now.
    pub blocked: Option<Divergence>,
}

impl AppTarget {
    pub fn runnable(&self) -> bool {
        self.blocked.is_none()
    }
}

/// Resolve each enabled app to its target output, using the outputs the plan made live.
pub fn resolve_app_targets(
    apps: &[AppConfig],
    observed: &[Output],
    workspaces: &HashMap<String, u32>,
) -> Vec<AppTarget> {
    apps.iter()
        .filter(|app| app.enabled)
        .map(|app| {
            let Some(rule) = &app.output else {
                // No pinned output: Sway's default placement applies.
                return AppTarget {
                    id: app.id.clone(),
                    output: None,
                    workspace: None,
                    blocked: None,
                };
            };

            let matched = observed.iter().find(|output| rule.matches(output));
            match matched {
                Some(output) if workspaces.contains_key(&output.name) => AppTarget {
                    id: app.id.clone(),
                    output: Some(output.name.clone()),
                    workspace: workspaces.get(&output.name).copied(),
                    blocked: None,
                },
                Some(output) => AppTarget {
                    id: app.id.clone(),
                    output: Some(output.name.clone()),
                    workspace: None,
                    blocked: Some(Divergence::new(
                        "app_output_disabled",
                        &app.id,
                        format!(
                            "{} targets {}, which is connected but not enabled",
                            app.id, output.name
                        ),
                    )),
                },
                None => AppTarget {
                    id: app.id.clone(),
                    output: None,
                    workspace: None,
                    blocked: Some(Divergence::new(
                        "app_waiting_for_output",
                        &app.id,
                        format!("{} targets {}, which is not connected", app.id, rule.key()),
                    )),
                },
            }
        })
        .collect()
}

/// Commands that place a window and make it fill its output — or every output.
///
/// `workspace` is optional: an app that spans every output pins no single one,
/// but it still needs its fullscreen mode set. Skipping placement whenever no
/// workspace was resolved would leave such a window with whatever fullscreen
/// state the client asked for — which for a kiosk browser is one output.
pub fn placement_commands(
    window_id: i64,
    workspace: Option<u32>,
    fullscreen: bool,
    span_outputs: bool,
) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(workspace) = workspace {
        commands.push(format!(
            "[con_id={window_id}] move container to workspace number {workspace}"
        ));
    }
    if span_outputs {
        // `global` stretches the window across the whole layout, which is what
        // drives a video wall from a single browser. Sway upgrades straight
        // from per-output fullscreen, so no need to clear it first.
        commands.push(format!("[con_id={window_id}] fullscreen enable global"));
    } else if fullscreen {
        // Without `global`, the window fills only its own output.
        commands.push(format!("[con_id={window_id}] fullscreen enable"));
    }
    commands
}

/// The `output … bg` command for a background, if it asks for anything.
///
/// Sway takes either a file with a scaling mode, or a solid colour. A missing
/// wallpaper is a divergence rather than a failure: the output keeps working,
/// and the operator is told which id could not be found.
fn background_command<F>(
    name: &str,
    background: &Background,
    resolve_wallpaper: &F,
) -> Result<Option<String>, Divergence>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(id) = &background.wallpaper {
        let Some(path) = resolve_wallpaper(id) else {
            return Err(Divergence::new(
                "wallpaper_not_found",
                name,
                format!("{name} refers to wallpaper {id:?}, which is not stored"),
            ));
        };
        // Sway paints the colour wherever the image does not reach — which is
        // every mode except `fill` and `stretch`, so it is always supplied.
        return Ok(Some(format!(
            "output {name} bg {path} {} #{}",
            background.mode.as_sway(),
            background.sway_color()
        )));
    }

    Ok(Some(format!(
        "output {name} bg #{} solid_color",
        background.sway_color()
    )))
}

/// The `fullscreen_mode` sway should report once placement has taken effect.
///
/// 0 = windowed, 1 = fills its output, 2 = spans the whole layout.
pub fn desired_fullscreen_mode(fullscreen: bool, span_outputs: bool) -> i32 {
    if span_outputs {
        2
    } else if fullscreen {
        1
    } else {
        0
    }
}

/// Hide the pointer and park it below the layout, out of every output.
///
/// Hiding alone is not enough on multi-output setups, where a resting pointer
/// can still keep an output awake, so it is also moved out of the way.
pub fn cursor_commands(layout_height: i32) -> Vec<String> {
    vec![
        "seat seat0 hide_cursor 1000".to_string(),
        format!("seat seat0 cursor set 0 {}", layout_height + 24),
    ]
}

fn format_number(value: f64) -> String {
    let text = format!("{value:.3}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BackgroundMode, Launcher, Mode, OutputMatch, Position, Rect, RestartPolicy, Transform,
    };

    fn output(name: &str, active: bool) -> Output {
        Output {
            name: name.into(),
            active,
            make: None,
            model: None,
            serial: None,
            current_mode: if active {
                Some(Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60.0,
                })
            } else {
                None
            },
            modes: if active {
                vec![
                    Mode {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60.0,
                    },
                    Mode {
                        width: 1280,
                        height: 720,
                        refresh_hz: 60.0,
                    },
                ]
            } else {
                vec![]
            },
            rect: if active {
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                }
            } else {
                Rect::default()
            },
            scale: Some(1.0),
            transform: Some("normal".into()),
            adaptive_sync_status: Some("disabled".into()),
        }
    }

    fn config(name: &str) -> OutputConfig {
        OutputConfig::new(OutputMatch::by_name(name))
    }

    fn tearing_capable() -> Capabilities {
        Capabilities {
            supports_tearing: true,
        }
    }

    fn app(id: &str, output: Option<&str>) -> AppConfig {
        AppConfig {
            id: id.into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "true".into(),
                args: vec![],
            },
            output: output.map(OutputMatch::by_name),
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        }
    }

    #[test]
    fn already_correct_output_needs_no_commands() {
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        desired.position = Some(Position { x: 0, y: 0 });
        desired.scale = Some(1.0);
        desired.transform = Some(Transform::Normal);

        // Seed what a previous pass applied, so unreported fields are known.
        let mut applied = HashMap::new();
        applied.insert(
            "HDMI-A-1".to_string(),
            AppliedOutput {
                background: None,
                config: desired.clone(),
                workspace: 1,
            },
        );

        let plan = plan_outputs(&observed, &[desired], &applied, tearing_capable());
        assert!(
            plan.commands.is_empty(),
            "expected no commands, got {:?}",
            plan.commands
        );
        assert!(plan.divergences.is_empty());
        assert!(!plan.topology_changed);
    }

    #[test]
    fn cold_boot_configures_everything_in_order() {
        let observed = vec![output("HDMI-A-1", false)];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        desired.position = Some(Position { x: 0, y: 0 });

        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert_eq!(
            plan.commands,
            vec![
                "output HDMI-A-1 enable",
                "output HDMI-A-1 mode 1920x1080@60Hz",
                "output HDMI-A-1 pos 0 0",
                "output HDMI-A-1 adaptive_sync off",
                "output HDMI-A-1 allow_tearing no",
                "output HDMI-A-1 max_render_time off",
                "workspace 1 output HDMI-A-1",
            ]
        );
        assert!(plan.topology_changed);
        assert_eq!(plan.workspaces.get("HDMI-A-1"), Some(&1));
    }

    #[test]
    fn only_the_changed_field_is_issued() {
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60.0,
        });
        let mut applied = HashMap::new();
        applied.insert(
            "HDMI-A-1".to_string(),
            AppliedOutput {
                background: None,
                config: desired.clone(),
                workspace: 1,
            },
        );

        let plan = plan_outputs(&observed, &[desired], &applied, tearing_capable());
        assert_eq!(plan.commands, vec!["output HDMI-A-1 mode 1280x720@60Hz"]);
        assert!(!plan.topology_changed);
    }

    #[test]
    fn missing_output_is_a_divergence_not_an_error() {
        let observed = vec![output("HDMI-A-1", true)];
        let plan = plan_outputs(
            &observed,
            &[config("HDMI-A-2")],
            &HashMap::new(),
            tearing_capable(),
        );
        assert!(plan.commands.is_empty());
        assert_eq!(plan.divergences.len(), 1);
        assert_eq!(plan.divergences[0].kind, "output_not_connected");
        assert_eq!(plan.divergences[0].subject, "HDMI-A-2");
    }

    #[test]
    fn a_near_refresh_rate_is_applied_as_advertised() {
        // The planner must issue the rate the display actually offers, so the
        // next pass recognises the result as already satisfied.
        let mut display = output("HDMI-A-1", true);
        display.modes = vec![Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 59.951,
        }];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60.0,
        });

        let plan = plan_outputs(
            std::slice::from_ref(&display),
            std::slice::from_ref(&desired),
            &HashMap::new(),
            tearing_capable(),
        );
        assert!(
            plan.commands
                .contains(&"output HDMI-A-1 mode 2560x1440@59.951Hz".to_string()),
            "expected the advertised rate, got {:?}",
            plan.commands
        );
        assert!(plan.divergences.is_empty());

        // Once applied, the pass must settle rather than re-issuing forever.
        display.current_mode = Some(Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 59.951,
        });
        let second = plan_outputs(&[display], &[desired], &plan.applied, tearing_capable());
        assert!(
            !second.commands.iter().any(|c| c.contains("mode")),
            "second pass re-issued the mode: {:?}",
            second.commands
        );
    }

    #[test]
    fn unsupported_mode_is_reported_and_not_attempted() {
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 5120,
            height: 2880,
            refresh_hz: 60.0,
        });
        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert!(!plan.commands.iter().any(|c| c.contains("mode")));
        assert!(plan
            .divergences
            .iter()
            .any(|d| d.kind == "mode_unsupported"));
    }

    #[test]
    fn disable_is_issued_only_when_the_output_is_active() {
        let mut desired = config("HDMI-A-1");
        desired.enable = false;

        let active = vec![output("HDMI-A-1", true)];
        let plan = plan_outputs(
            &active,
            &[desired.clone()],
            &HashMap::new(),
            tearing_capable(),
        );
        assert_eq!(plan.commands, vec!["output HDMI-A-1 disable"]);
        assert!(plan.topology_changed);

        let inactive = vec![output("HDMI-A-1", false)];
        let plan = plan_outputs(&inactive, &[desired], &HashMap::new(), tearing_capable());
        assert!(plan.commands.is_empty());
        assert!(!plan.topology_changed);
    }

    #[test]
    fn connected_output_without_a_desired_entry_is_untouched() {
        let observed = vec![output("HDMI-A-1", true), output("HDMI-A-2", true)];
        let plan = plan_outputs(
            &observed,
            &[config("HDMI-A-1")],
            &HashMap::new(),
            tearing_capable(),
        );
        assert!(!plan.commands.iter().any(|c| c.contains("HDMI-A-2")));
        assert!(plan.divergences.is_empty());
    }

    #[test]
    fn tearing_uses_the_subcommand_sway_accepts() {
        // sway rejects a plain `tearing` with "Invalid output subcommand".
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.allow_tearing = true;

        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert!(plan
            .commands
            .contains(&"output HDMI-A-1 allow_tearing yes".to_string()));
        assert!(
            !plan.commands.iter().any(|c| c.contains(" tearing ")),
            "must not emit the rejected spelling: {:?}",
            plan.commands
        );
    }

    #[test]
    fn an_unmanaged_output_has_every_setting_applied_once() {
        // An observed position Suede did not set is not pinned: sway
        // auto-arranges such outputs and silently recomputes their position
        // when a neighbour changes size. So the first pass must be explicit
        // even where observed already equals desired.
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        // Exactly what the output already reports.
        desired.position = Some(Position { x: 0, y: 0 });
        desired.scale = Some(1.0);
        desired.transform = Some(Transform::Normal);

        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert!(
            plan.commands
                .contains(&"output HDMI-A-1 pos 0 0".to_string()),
            "position must be pinned on the first pass: {:?}",
            plan.commands
        );
        assert!(plan.commands.iter().any(|c| c.contains("mode")));
        assert!(plan.commands.iter().any(|c| c.contains("scale")));
        assert!(plan.commands.iter().any(|c| c.contains("transform")));
    }

    #[test]
    fn a_managed_output_returns_to_minimal_diffs() {
        // Having pinned everything once, the next pass must be quiet again.
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.position = Some(Position { x: 0, y: 0 });

        let first = plan_outputs(
            &observed,
            std::slice::from_ref(&desired),
            &HashMap::new(),
            tearing_capable(),
        );
        assert!(!first.commands.is_empty());

        let second = plan_outputs(&observed, &[desired], &first.applied, tearing_capable());
        assert!(
            second.commands.is_empty(),
            "second pass should be a no-op, got {:?}",
            second.commands
        );
    }

    #[test]
    fn tearing_is_skipped_and_reported_on_older_sway() {
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.allow_tearing = true;

        let plan = plan_outputs(
            &observed,
            &[desired],
            &HashMap::new(),
            Capabilities {
                supports_tearing: false,
            },
        );
        assert!(!plan.commands.iter().any(|c| c.contains("tearing")));
        assert!(plan
            .divergences
            .iter()
            .any(|d| d.kind == "tearing_unsupported"));
    }

    #[test]
    fn re_enabling_reapplies_every_setting() {
        // Sway forgets an output's settings when it is disabled, so a plan that
        // re-enables must not skip fields that "already match" a stale record.
        let observed = vec![output("HDMI-A-1", false)];
        let mut desired = config("HDMI-A-1");
        desired.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        desired.position = Some(Position { x: 0, y: 0 });
        let mut applied = HashMap::new();
        applied.insert(
            "HDMI-A-1".to_string(),
            AppliedOutput {
                background: None,
                config: desired.clone(),
                workspace: 1,
            },
        );

        let plan = plan_outputs(&observed, &[desired], &applied, tearing_capable());
        assert!(plan.commands.iter().any(|c| c.ends_with("enable")));
        assert!(plan.commands.iter().any(|c| c.contains("mode")));
        assert!(plan.commands.iter().any(|c| c.contains("pos")));
        assert!(plan.commands.iter().any(|c| c.contains("max_render_time")));
    }

    #[test]
    fn workspaces_follow_declaration_order() {
        let observed = vec![output("HDMI-A-1", true), output("HDMI-A-2", true)];
        let plan = plan_outputs(
            &observed,
            &[config("HDMI-A-2"), config("HDMI-A-1")],
            &HashMap::new(),
            tearing_capable(),
        );
        assert_eq!(plan.workspaces.get("HDMI-A-2"), Some(&1));
        assert_eq!(plan.workspaces.get("HDMI-A-1"), Some(&2));
    }

    #[test]
    fn max_render_time_is_formatted_as_off_or_milliseconds() {
        let observed = vec![output("HDMI-A-1", true)];
        let mut desired = config("HDMI-A-1");
        desired.max_render_time_ms = Some(9);
        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert!(plan
            .commands
            .contains(&"output HDMI-A-1 max_render_time 9".to_string()));
    }

    #[test]
    fn app_without_an_output_is_runnable() {
        let targets = resolve_app_targets(&[app("a", None)], &[], &HashMap::new());
        assert!(targets[0].runnable());
        assert_eq!(targets[0].workspace, None);
    }

    #[test]
    fn app_resolves_to_its_pinned_output() {
        let observed = vec![output("HDMI-A-1", true)];
        let mut workspaces = HashMap::new();
        workspaces.insert("HDMI-A-1".to_string(), 1);
        let targets = resolve_app_targets(&[app("a", Some("HDMI-A-1"))], &observed, &workspaces);
        assert!(targets[0].runnable());
        assert_eq!(targets[0].output.as_deref(), Some("HDMI-A-1"));
        assert_eq!(targets[0].workspace, Some(1));
    }

    #[test]
    fn app_is_blocked_when_its_output_is_absent() {
        let targets = resolve_app_targets(&[app("a", Some("HDMI-A-9"))], &[], &HashMap::new());
        assert!(!targets[0].runnable());
        assert_eq!(
            targets[0].blocked.as_ref().unwrap().kind,
            "app_waiting_for_output"
        );
    }

    #[test]
    fn app_is_blocked_when_its_output_is_connected_but_disabled() {
        let observed = vec![output("HDMI-A-1", true)];
        let targets =
            resolve_app_targets(&[app("a", Some("HDMI-A-1"))], &observed, &HashMap::new());
        assert!(!targets[0].runnable());
        assert_eq!(
            targets[0].blocked.as_ref().unwrap().kind,
            "app_output_disabled"
        );
    }

    #[test]
    fn disabled_apps_are_not_targets() {
        let mut disabled = app("a", None);
        disabled.enabled = false;
        assert!(resolve_app_targets(&[disabled], &[], &HashMap::new()).is_empty());
    }

    #[test]
    fn placement_uses_per_output_fullscreen() {
        let commands = placement_commands(42, Some(3), true, false);
        assert_eq!(
            commands,
            vec![
                "[con_id=42] move container to workspace number 3",
                "[con_id=42] fullscreen enable",
            ]
        );
        // `global` would span every output, which is not what a per-output kiosk wants.
        assert!(!commands.iter().any(|c| c.contains("global")));
    }

    #[test]
    fn placement_can_skip_fullscreen() {
        assert_eq!(placement_commands(42, Some(3), false, false).len(), 1);
    }

    #[test]
    fn spanning_uses_global_fullscreen() {
        // One browser across every output: the video-wall case.
        let commands = placement_commands(42, Some(1), true, true);
        assert_eq!(
            commands,
            vec![
                "[con_id=42] move container to workspace number 1",
                "[con_id=42] fullscreen enable global",
            ]
        );
    }

    #[test]
    fn a_spanning_app_is_placed_even_with_no_pinned_output() {
        // The regression that made spanning silently fill one monitor: with no
        // workspace resolved, placement produced nothing at all and the kiosk
        // browser kept its own per-output fullscreen.
        let commands = placement_commands(9, None, true, true);
        assert_eq!(commands, vec!["[con_id=9] fullscreen enable global"]);
        assert!(!commands.is_empty(), "spanning must still be applied");
    }

    #[test]
    fn without_a_workspace_nothing_is_moved() {
        let commands = placement_commands(9, None, true, false);
        assert!(!commands.iter().any(|c| c.contains("move container")));
        assert_eq!(commands, vec!["[con_id=9] fullscreen enable"]);
    }

    #[test]
    fn desired_modes_match_sways_numbering() {
        assert_eq!(desired_fullscreen_mode(true, true), 2);
        assert_eq!(desired_fullscreen_mode(true, false), 1);
        assert_eq!(desired_fullscreen_mode(false, false), 0);
        // Spanning implies filling the screen even if fullscreen is unset.
        assert_eq!(desired_fullscreen_mode(false, true), 2);
    }

    #[test]
    fn spanning_wins_over_per_output_fullscreen() {
        // `global` already implies filling the screen; issuing both would
        // leave the window merely filling one output.
        let commands = placement_commands(7, Some(2), false, true);
        assert!(commands
            .iter()
            .any(|c| c.ends_with("fullscreen enable global")));
        assert_eq!(
            commands.iter().filter(|c| c.contains("fullscreen")).count(),
            1
        );
    }

    use crate::model::BackgroundRef;

    fn with_background(name: &str, background: Background) -> OutputConfig {
        let mut config = config(name);
        config.background = Some(BackgroundRef::Inline(background));
        config
    }

    #[test]
    fn a_colour_background_is_a_solid_colour() {
        let observed = vec![output("HDMI-A-1", true)];
        let desired = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: None,
                color: Some("#101820".into()),
                mode: BackgroundMode::Fill,
            },
        );
        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert!(plan
            .commands
            .contains(&"output HDMI-A-1 bg #101820 solid_color".to_string()));
        assert!(plan.divergences.is_empty());
    }

    #[test]
    fn a_wallpaper_background_names_the_file_and_mode() {
        let observed = vec![output("HDMI-A-1", true)];
        let desired = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: Some("lobby".into()),
                color: None,
                mode: BackgroundMode::Fit,
            },
        );
        let plan = plan_outputs_with(
            &observed,
            &[desired],
            &[],
            &HashMap::new(),
            tearing_capable(),
            |id| Some(format!("/state/wallpapers/{id}.png")),
        );
        // The colour is always supplied: `fit` letterboxes, and an unstated
        // colour would leave swaybg to choose what the bars look like.
        assert!(plan
            .commands
            .contains(&"output HDMI-A-1 bg /state/wallpapers/lobby.png fit #000000".to_string()));
    }

    #[test]
    fn an_unstated_colour_is_black_rather_than_nothing() {
        let observed = vec![output("HDMI-A-1", true)];
        let desired = with_background("HDMI-A-1", Background::default());
        let plan = plan_outputs(&observed, &[desired], &HashMap::new(), tearing_capable());
        assert!(
            plan.commands
                .contains(&"output HDMI-A-1 bg #000000 solid_color".to_string()),
            "an empty background should still paint the screen: {:?}",
            plan.commands
        );
    }

    // --- presets ---------------------------------------------------------

    fn preset(id: &str, background: Background) -> crate::model::BackgroundPreset {
        crate::model::BackgroundPreset {
            id: id.to_string(),
            background,
        }
    }

    fn referencing(name: &str, preset_id: &str) -> OutputConfig {
        let mut config = config(name);
        config.background = Some(BackgroundRef::Preset(preset_id.to_string()));
        config
    }

    #[test]
    fn an_output_can_name_a_preset_instead_of_repeating_it() {
        let observed = vec![output("HDMI-A-1", true)];
        let presets = vec![preset(
            "lobby",
            Background {
                wallpaper: Some("art".into()),
                color: Some("#101820".into()),
                mode: BackgroundMode::Fit,
            },
        )];
        let plan = plan_outputs_with(
            &observed,
            &[referencing("HDMI-A-1", "lobby")],
            &presets,
            &HashMap::new(),
            tearing_capable(),
            |id| Some(format!("/w/{id}.png")),
        );
        assert!(plan
            .commands
            .contains(&"output HDMI-A-1 bg /w/art.png fit #101820".to_string()));
        assert!(plan.divergences.is_empty());
    }

    #[test]
    fn one_preset_paints_every_output_that_names_it() {
        // The whole point: a video wall is configured once, not once per screen.
        let observed = vec![output("HDMI-A-1", true), output("HDMI-A-2", true)];
        let presets = vec![preset(
            "wall",
            Background {
                wallpaper: None,
                color: Some("#223344".into()),
                mode: BackgroundMode::Fill,
            },
        )];
        let plan = plan_outputs_with(
            &observed,
            &[
                referencing("HDMI-A-1", "wall"),
                referencing("HDMI-A-2", "wall"),
            ],
            &presets,
            &HashMap::new(),
            tearing_capable(),
            |_| None,
        );
        for name in ["HDMI-A-1", "HDMI-A-2"] {
            assert!(
                plan.commands
                    .contains(&format!("output {name} bg #223344 solid_color")),
                "{name} was not painted: {:?}",
                plan.commands
            );
        }
    }

    #[test]
    fn editing_a_preset_repaints_the_outputs_using_it() {
        // The reference is unchanged, so diffing it would conclude nothing has
        // happened and leave the old picture on the wall.
        let observed = vec![output("HDMI-A-1", true)];
        let desired = vec![referencing("HDMI-A-1", "lobby")];
        let before = vec![preset(
            "lobby",
            Background {
                wallpaper: None,
                color: Some("#111111".into()),
                mode: BackgroundMode::Fill,
            },
        )];
        let first = plan_outputs_with(
            &observed,
            &desired,
            &before,
            &HashMap::new(),
            tearing_capable(),
            |_| None,
        );

        let after = vec![preset(
            "lobby",
            Background {
                wallpaper: None,
                color: Some("#999999".into()),
                mode: BackgroundMode::Fill,
            },
        )];
        let second = plan_outputs_with(
            &observed,
            &desired,
            &after,
            &HashMap::new(),
            tearing_capable(),
            |_| None,
        );
        assert!(first
            .commands
            .contains(&"output HDMI-A-1 bg #111111 solid_color".to_string()));
        assert!(
            second
                .commands
                .contains(&"output HDMI-A-1 bg #999999 solid_color".to_string()),
            "editing the preset must reach the screen: {:?}",
            second.commands
        );
    }

    #[test]
    fn an_unchanged_preset_is_not_repainted_every_pass() {
        let observed = vec![output("HDMI-A-1", true)];
        let desired = vec![referencing("HDMI-A-1", "lobby")];
        let presets = vec![preset(
            "lobby",
            Background {
                wallpaper: None,
                color: Some("#111111".into()),
                mode: BackgroundMode::Fill,
            },
        )];
        let first = plan_outputs_with(
            &observed,
            &desired,
            &presets,
            &HashMap::new(),
            tearing_capable(),
            |_| None,
        );
        let second = plan_outputs_with(
            &observed,
            &desired,
            &presets,
            &first.applied,
            tearing_capable(),
            |_| None,
        );
        assert!(!second.commands.iter().any(|c| c.contains(" bg ")));
    }

    #[test]
    fn a_preset_that_does_not_exist_is_a_divergence() {
        let observed = vec![output("HDMI-A-1", true)];
        let plan = plan_outputs_with(
            &observed,
            &[referencing("HDMI-A-1", "typo")],
            &[],
            &HashMap::new(),
            tearing_capable(),
            |_| None,
        );
        assert!(!plan.commands.iter().any(|c| c.contains(" bg ")));
        let divergence = plan
            .divergences
            .iter()
            .find(|d| d.kind == "background_preset_not_found")
            .expect("the operator must be told which name is wrong");
        assert!(divergence.detail.contains("typo"));
    }

    #[test]
    fn a_wallpaper_can_carry_a_fallback_colour() {
        // `fit` letterboxes, so the colour decides what the bars look like.
        let observed = vec![output("HDMI-A-1", true)];
        let desired = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: Some("lobby".into()),
                color: Some("#000000".into()),
                mode: BackgroundMode::Fit,
            },
        );
        let plan = plan_outputs_with(
            &observed,
            &[desired],
            &[],
            &HashMap::new(),
            tearing_capable(),
            |_| Some("/w/lobby.png".to_string()),
        );
        assert!(plan
            .commands
            .contains(&"output HDMI-A-1 bg /w/lobby.png fit #000000".to_string()));
    }

    #[test]
    fn a_missing_wallpaper_is_a_divergence_not_a_broken_command() {
        let observed = vec![output("HDMI-A-1", true)];
        let desired = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: Some("gone".into()),
                color: None,
                mode: BackgroundMode::Fill,
            },
        );
        let plan = plan_outputs_with(
            &observed,
            &[desired],
            &[],
            &HashMap::new(),
            tearing_capable(),
            |_| None,
        );
        assert!(!plan.commands.iter().any(|c| c.contains(" bg ")));
        assert!(plan
            .divergences
            .iter()
            .any(|d| d.kind == "wallpaper_not_found"));
    }

    #[test]
    fn an_unchanged_background_is_not_reapplied() {
        // Sway does not report the background back, so this is diffed against
        // what Suede last applied; without that it would be re-issued forever.
        let observed = vec![output("HDMI-A-1", true)];
        let desired = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: None,
                color: Some("#101820".into()),
                mode: BackgroundMode::Fill,
            },
        );
        let first = plan_outputs(
            &observed,
            std::slice::from_ref(&desired),
            &HashMap::new(),
            tearing_capable(),
        );
        assert!(first.commands.iter().any(|c| c.contains(" bg ")));

        let second = plan_outputs(&observed, &[desired], &first.applied, tearing_capable());
        assert!(
            !second.commands.iter().any(|c| c.contains(" bg ")),
            "second pass re-applied the background: {:?}",
            second.commands
        );
    }

    #[test]
    fn changing_the_background_reapplies_it() {
        let observed = vec![output("HDMI-A-1", true)];
        let before = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: None,
                color: Some("#101820".into()),
                mode: BackgroundMode::Fill,
            },
        );
        let first = plan_outputs(&observed, &[before], &HashMap::new(), tearing_capable());

        let after = with_background(
            "HDMI-A-1",
            Background {
                wallpaper: None,
                color: Some("#204060".into()),
                mode: BackgroundMode::Fill,
            },
        );
        let second = plan_outputs(&observed, &[after], &first.applied, tearing_capable());
        assert!(second
            .commands
            .contains(&"output HDMI-A-1 bg #204060 solid_color".to_string()));
    }

    #[test]
    fn cursor_is_hidden_and_parked_below_the_layout() {
        let commands = cursor_commands(1080);
        assert_eq!(commands[0], "seat seat0 hide_cursor 1000");
        assert_eq!(commands[1], "seat seat0 cursor set 0 1104");
    }

    #[test]
    fn scale_is_formatted_without_trailing_zeros() {
        assert_eq!(format_number(1.0), "1");
        assert_eq!(format_number(1.5), "1.5");
        assert_eq!(format_number(1.25), "1.25");
    }
}
