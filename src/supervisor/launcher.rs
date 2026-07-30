//! Turning a launcher specification into a concrete process invocation.
//!
//! Pure and testable: no process is spawned here.

use std::path::{Path, PathBuf};

use crate::audio::resolve_pulse_sink;
use crate::model::{AppConfig, Launcher};

/// Chromium arguments for an unattended kiosk, carried over from the
/// production .NET service this project generalizes.
const CHROMIUM_KIOSK_ARGS: &[&str] = &[
    // Avoid the "unlock keyring" dialog on a headless appliance.
    "--password-store=basic",
    "--kiosk",
    "--start-fullscreen",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-infobars",
    "--disable-session-crashed-bubble",
    "--disable-restore-session-state",
    "--disable-features=TranslateUI",
    "--disable-ipc-flooding-protection",
    "--aggressive-cache-discard",
    "--app-auto-launched",
    "--force-device-scale-factor=1",
    "--ozone-platform=wayland",
    "--enable-features=VaapiVideoDecoder,Vulkan,CanvasOopRasterization",
    "--ignore-gpu-blocklist",
    "--enable-zero-copy",
];

const FIREFOX_KIOSK_ARGS: &[&str] = &["--kiosk", "--new-instance", "--private-window"];

/// Everything needed to spawn one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Browser profile directory, wiped before launch unless the app opts out.
    pub profile_dir: Option<PathBuf>,
    pub wipe_profile: bool,
}

/// Deployment facts the launcher needs.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    /// Root under which per-app browser profiles live.
    pub profiles_root: PathBuf,
    /// Loopback base for the API, e.g. `http://127.0.0.1:7071/api/v1`.
    pub api_base: String,
}

impl LaunchContext {
    pub fn heartbeat_url(&self, app_id: &str) -> String {
        format!(
            "{}/apps/{}/heartbeat",
            self.api_base.trim_end_matches('/'),
            app_id
        )
    }
}

/// Substitute the placeholders a URI may carry.
pub fn expand_uri(uri: &str, app_id: &str, context: &LaunchContext) -> String {
    uri.replace("{appId}", app_id)
        .replace("{heartbeatUrl}", &context.heartbeat_url(app_id))
}

/// Build the invocation for `app`.
pub fn build(app: &AppConfig, context: &LaunchContext) -> LaunchSpec {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(sink) = resolve_pulse_sink(app.audio.as_ref()) {
        env.push(("PULSE_SINK".to_string(), sink));
    }

    match &app.launcher {
        Launcher::ChromiumKiosk {
            uri,
            show_fps_counter,
            extra_args,
        } => {
            let profile_dir = profile_dir_for(&context.profiles_root, &app.id);
            let mut args: Vec<String> = CHROMIUM_KIOSK_ARGS.iter().map(|a| a.to_string()).collect();
            // Chromium refuses a second instance sharing a profile, so every
            // app gets its own.
            args.push(format!("--user-data-dir={}", profile_dir.display()));
            if *show_fps_counter {
                args.push("--show-fps-counter".to_string());
            }
            args.extend(extra_args.iter().cloned());
            args.push(expand_uri(uri, &app.id, context));

            LaunchSpec {
                program: "chromium".to_string(),
                args,
                env,
                profile_dir: Some(profile_dir),
                wipe_profile: !app.persist_profile,
            }
        }
        Launcher::FirefoxKiosk { uri, extra_args } => {
            let mut args: Vec<String> = FIREFOX_KIOSK_ARGS.iter().map(|a| a.to_string()).collect();
            args.extend(extra_args.iter().cloned());
            args.push(expand_uri(uri, &app.id, context));
            // Firefox needs telling to use Wayland rather than XWayland.
            env.push(("MOZ_ENABLE_WAYLAND".to_string(), "1".to_string()));

            LaunchSpec {
                program: "firefox".to_string(),
                args,
                env,
                profile_dir: None,
                wipe_profile: false,
            }
        }
        Launcher::Exec { command, args } => LaunchSpec {
            program: command.clone(),
            args: args
                .iter()
                .map(|arg| expand_uri(arg, &app.id, context))
                .collect(),
            env,
            profile_dir: None,
            wipe_profile: false,
        },
    }
}

fn profile_dir_for(root: &Path, app_id: &str) -> PathBuf {
    root.join(app_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioConfig, RestartPolicy};

    fn context() -> LaunchContext {
        LaunchContext {
            profiles_root: PathBuf::from("/state/profiles"),
            api_base: "http://127.0.0.1:7071/api/v1".into(),
        }
    }

    fn app(id: &str, launcher: Launcher) -> AppConfig {
        AppConfig {
            id: id.into(),
            enabled: true,
            launcher,
            output: None,
            fullscreen: true,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        }
    }

    fn chromium(uri: &str) -> Launcher {
        Launcher::ChromiumKiosk {
            uri: uri.into(),
            show_fps_counter: false,
            extra_args: vec![],
        }
    }

    #[test]
    fn chromium_preset_is_kiosk_and_wayland() {
        let spec = build(&app("r1", chromium("http://example.com")), &context());
        assert_eq!(spec.program, "chromium");
        assert!(spec.args.iter().any(|a| a == "--kiosk"));
        assert!(spec.args.iter().any(|a| a == "--ozone-platform=wayland"));
        assert!(spec.args.iter().any(|a| a == "--password-store=basic"));
    }

    #[test]
    fn uri_is_the_last_argument() {
        let spec = build(&app("r1", chromium("http://example.com/page")), &context());
        assert_eq!(spec.args.last().unwrap(), "http://example.com/page");
    }

    #[test]
    fn each_app_gets_its_own_profile() {
        let first = build(&app("r1", chromium("http://a")), &context());
        let second = build(&app("r2", chromium("http://a")), &context());
        assert_ne!(first.profile_dir, second.profile_dir);
        assert!(first
            .args
            .iter()
            .any(|a| a == "--user-data-dir=/state/profiles/r1"));
        assert!(first.wipe_profile);
    }

    #[test]
    fn persist_profile_opts_out_of_wiping() {
        let mut config = app("r1", chromium("http://a"));
        config.persist_profile = true;
        let spec = build(&config, &context());
        assert!(!spec.wipe_profile);
        assert!(spec.profile_dir.is_some());
    }

    #[test]
    fn fps_counter_is_opt_in() {
        let without = build(&app("r1", chromium("http://a")), &context());
        assert!(!without.args.iter().any(|a| a == "--show-fps-counter"));

        let with = build(
            &app(
                "r1",
                Launcher::ChromiumKiosk {
                    uri: "http://a".into(),
                    show_fps_counter: true,
                    extra_args: vec![],
                },
            ),
            &context(),
        );
        assert!(with.args.iter().any(|a| a == "--show-fps-counter"));
    }

    #[test]
    fn extra_args_come_before_the_uri() {
        let spec = build(
            &app(
                "r1",
                Launcher::ChromiumKiosk {
                    uri: "http://a".into(),
                    show_fps_counter: false,
                    extra_args: vec!["--mute-audio".into()],
                },
            ),
            &context(),
        );
        let mute = spec.args.iter().position(|a| a == "--mute-audio").unwrap();
        let uri = spec.args.iter().position(|a| a == "http://a").unwrap();
        assert!(mute < uri);
    }

    #[test]
    fn firefox_preset_enables_wayland_by_environment() {
        let spec = build(
            &app(
                "r1",
                Launcher::FirefoxKiosk {
                    uri: "http://a".into(),
                    extra_args: vec![],
                },
            ),
            &context(),
        );
        assert_eq!(spec.program, "firefox");
        assert!(spec.args.iter().any(|a| a == "--kiosk"));
        assert!(spec
            .env
            .iter()
            .any(|(key, value)| key == "MOZ_ENABLE_WAYLAND" && value == "1"));
        assert!(spec.profile_dir.is_none());
    }

    #[test]
    fn exec_launcher_is_verbatim() {
        let spec = build(
            &app(
                "r1",
                Launcher::Exec {
                    command: "/usr/bin/mpv".into(),
                    args: vec!["--fullscreen".into(), "video.mp4".into()],
                },
            ),
            &context(),
        );
        assert_eq!(spec.program, "/usr/bin/mpv");
        assert_eq!(spec.args, vec!["--fullscreen", "video.mp4"]);
    }

    #[test]
    fn placeholders_are_expanded() {
        let spec = build(
            &app(
                "renderer-3",
                chromium("http://host/r?id={appId}&hb={heartbeatUrl}"),
            ),
            &context(),
        );
        let uri = spec.args.last().unwrap();
        assert!(uri.contains("id=renderer-3"));
        assert!(uri.contains("hb=http://127.0.0.1:7071/api/v1/apps/renderer-3/heartbeat"));
        assert!(!uri.contains('{'));
    }

    #[test]
    fn audio_routing_is_applied_by_environment() {
        let mut config = app("r1", chromium("http://a"));
        config.audio = Some(AudioConfig {
            output: Some("alsa_output.hdmi".into()),
        });
        let spec = build(&config, &context());
        assert!(spec
            .env
            .iter()
            .any(|(key, value)| key == "PULSE_SINK" && value == "alsa_output.hdmi"));
    }

    #[test]
    fn absent_audio_config_sets_no_sink() {
        let spec = build(&app("r1", chromium("http://a")), &context());
        assert!(!spec.env.iter().any(|(key, _)| key == "PULSE_SINK"));
    }

    #[test]
    fn null_audio_routes_to_the_null_sink() {
        let mut config = app("r1", chromium("http://a"));
        config.audio = Some(AudioConfig { output: None });
        let spec = build(&config, &context());
        assert!(spec
            .env
            .iter()
            .any(|(key, value)| key == "PULSE_SINK" && value == crate::audio::NULL_SINK_NAME));
    }
}
