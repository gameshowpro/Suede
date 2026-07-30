//! Pure planning: given observed and desired state, produce the exact commands
//! needed to close the gap.
//!
//! This module performs no IO, which is what makes the reconciliation rules
//! testable without a compositor. The executor in [`super`] runs the plan.

use std::collections::HashMap;

use crate::model::{AppConfig, Divergence, Output, OutputConfig};

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

        // Enabling an output resets everything Sway knows about it, so once we
        // issue `enable` every other setting must be re-applied unconditionally.
        let just_enabled = !output.active;
        if just_enabled {
            plan.commands.push(format!("output {name} enable"));
            plan.topology_changed = true;
        }

        if let Some(mode) = config.mode {
            let advertises_modes = !output.modes.is_empty();
            if output.active && advertises_modes && !output.supports(&mode) {
                plan.divergences.push(Divergence::new(
                    "mode_unsupported",
                    name,
                    format!("{name} does not advertise mode {}", mode.to_sway()),
                ));
            } else if just_enabled
                || output
                    .current_mode
                    .is_none_or(|current| !current.matches(&mode))
            {
                plan.commands
                    .push(format!("output {name} mode {}", mode.to_sway()));
            }
        }

        if let Some(position) = config.position {
            if just_enabled || output.rect.x != position.x || output.rect.y != position.y {
                plan.commands
                    .push(format!("output {name} pos {} {}", position.x, position.y));
            }
        }

        if let Some(scale) = config.scale {
            let differs = output
                .scale
                .is_none_or(|current| (current - scale).abs() > 1e-6);
            if just_enabled || differs {
                plan.commands
                    .push(format!("output {name} scale {}", format_number(scale)));
            }
        }

        if let Some(transform) = config.transform {
            let wanted = transform.as_sway();
            if just_enabled || output.transform.as_deref() != Some(wanted) {
                plan.commands
                    .push(format!("output {name} transform {wanted}"));
            }
        }

        let adaptive_sync_active = output
            .adaptive_sync_status
            .as_deref()
            .map(|status| status == "enabled");
        if just_enabled || adaptive_sync_active != Some(config.adaptive_sync) {
            plan.commands.push(format!(
                "output {name} adaptive_sync {}",
                if config.adaptive_sync { "on" } else { "off" }
            ));
        }

        let applied = previously_applied.get(name);

        if capabilities.supports_tearing {
            let changed = applied.is_none_or(|a| a.config.allow_tearing != config.allow_tearing);
            if just_enabled || changed {
                plan.commands.push(format!(
                    "output {name} tearing {}",
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
        if just_enabled || render_time_changed {
            let value = config
                .max_render_time_ms
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| "off".to_string());
            plan.commands
                .push(format!("output {name} max_render_time {value}"));
        }

        // Pinning a workspace to the output makes app placement deterministic.
        if just_enabled || applied.is_none_or(|a| a.workspace != workspace) {
            plan.commands
                .push(format!("workspace {workspace} output {name}"));
        }

        plan.workspaces.insert(name.to_string(), workspace);
        plan.applied.insert(
            name.to_string(),
            AppliedOutput {
                config: config.clone(),
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

/// Commands that place a window and make it fill its output.
pub fn placement_commands(window_id: i64, workspace: u32, fullscreen: bool) -> Vec<String> {
    let mut commands = vec![format!(
        "[con_id={window_id}] move container to workspace number {workspace}"
    )];
    if fullscreen {
        // Not `global`: each app fills its own output, not the whole layout.
        commands.push(format!("[con_id={window_id}] fullscreen enable"));
    }
    commands
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
    use crate::model::{Launcher, Mode, OutputMatch, Position, Rect, RestartPolicy, Transform};

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
                "output HDMI-A-1 tearing no",
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
        let commands = placement_commands(42, 3, true);
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
        assert_eq!(placement_commands(42, 3, false).len(), 1);
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
