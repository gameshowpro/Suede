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
    // Vulkan is deliberately absent: Chromium rejects it under
    // `--ozone-platform=wayland` ("not compatible with Vulkan") and logs an
    // error on every launch. The .NET service this preset came from carried
    // the flag without it ever taking effect.
    "--enable-features=VaapiVideoDecoder,CanvasOopRasterization",
    "--ignore-gpu-blocklist",
    "--enable-zero-copy",
];

const FIREFOX_KIOSK_ARGS: &[&str] = &["--kiosk", "--new-instance", "--private-window"];

/// Binary names a Chromium-family browser may go by, most specific first.
///
/// There is no single answer: Debian and Arch ship `chromium`, Raspberry Pi OS
/// and older Ubuntu ship `chromium-browser`, Google's own package installs
/// `google-chrome-stable`, and Ubuntu's snap only appears under `/snap/bin`.
/// Hardcoding one name makes the preset silently unusable on most machines.
pub const CHROMIUM_PROGRAMS: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
    "/snap/bin/chromium",
];

/// Firefox is packaged as `firefox`, or `firefox-esr` on Debian.
pub const FIREFOX_PROGRAMS: &[&str] = &["firefox", "firefox-esr"];

/// Everything needed to spawn one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Program names to try in order; the first one found is used.
    pub programs: Vec<String>,
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
    /// Root under which per-app stderr logs are written.
    pub log_root: PathBuf,
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

    /// Where an app's stderr is captured, so a failure can be explained.
    pub fn log_path(&self, app_id: &str) -> PathBuf {
        self.log_root.join(format!("{app_id}.log"))
    }
}

/// Substitute the placeholders a URI may carry.
pub fn expand_uri(uri: &str, app_id: &str, context: &LaunchContext) -> String {
    uri.replace("{appId}", app_id)
        .replace("{heartbeatUrl}", &context.heartbeat_url(app_id))
}

/// Build the invocation for `app`.
pub fn build(app: &AppConfig, context: &LaunchContext) -> LaunchSpec {
    let mut spec = build_preset(app, context);
    // The app's own variables go last, so an operator can override anything a
    // preset chose — including the audio sink.
    spec.env
        .extend(app.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    spec
}

fn build_preset(app: &AppConfig, context: &LaunchContext) -> LaunchSpec {
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
                programs: CHROMIUM_PROGRAMS.iter().map(|p| p.to_string()).collect(),
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
                programs: FIREFOX_PROGRAMS.iter().map(|p| p.to_string()).collect(),
                args,
                env,
                profile_dir: None,
                wipe_profile: false,
            }
        }
        Launcher::Exec { command, args } => LaunchSpec {
            programs: vec![command.clone()],
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

/// The first candidate that exists, searching `$PATH` for bare names.
///
/// `lookup` decides whether a resolved path is usable, so the search itself
/// can be tested without depending on what happens to be installed.
pub fn resolve_program_with<F>(
    candidates: &[String],
    path_var: Option<&str>,
    exists: F,
) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.is_absolute() || candidate.contains('/') {
            if exists(path) {
                return Some(path.to_path_buf());
            }
            continue;
        }
        for dir in path_var
            .unwrap_or_default()
            .split(':')
            .filter(|d| !d.is_empty())
        {
            let full = Path::new(dir).join(candidate);
            if exists(&full) {
                return Some(full);
            }
        }
    }
    None
}

/// The first candidate program actually present on this machine.
pub fn resolve_program(candidates: &[String]) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok();
    resolve_program_with(candidates, path_var.as_deref(), |path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioConfig, RestartPolicy};

    fn context() -> LaunchContext {
        LaunchContext {
            profiles_root: PathBuf::from("/state/profiles"),
            log_root: PathBuf::from("/state/logs"),
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
            span_outputs: false,
            env: Default::default(),
            readiness: None,
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
        assert_eq!(spec.programs[0], "chromium");
        assert!(spec.programs.iter().any(|p| p == "google-chrome-stable"));
        assert!(spec.args.iter().any(|a| a == "--kiosk"));
        assert!(spec.args.iter().any(|a| a == "--ozone-platform=wayland"));
        assert!(spec.args.iter().any(|a| a == "--password-store=basic"));
    }

    #[test]
    fn no_flag_conflicts_with_wayland() {
        let spec = build(&app("r1", chromium("http://example.com")), &context());
        let features = spec
            .args
            .iter()
            .find(|a| a.starts_with("--enable-features="))
            .expect("the preset enables features");
        // Chromium refuses Vulkan when ozone is targeting Wayland.
        assert!(
            !features.contains("Vulkan"),
            "Vulkan conflicts with --ozone-platform=wayland: {features}"
        );
        assert!(spec.args.iter().any(|a| a == "--ozone-platform=wayland"));
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
        assert_eq!(spec.programs[0], "firefox");
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
        assert_eq!(spec.programs, vec!["/usr/bin/mpv"]);
        assert_eq!(spec.args, vec!["--fullscreen", "video.mp4"]);
    }

    #[test]
    fn a_bare_name_is_found_on_path() {
        let candidates = vec!["chromium".to_string(), "google-chrome-stable".to_string()];
        // Only the second candidate exists on this imaginary machine.
        let found = resolve_program_with(&candidates, Some("/usr/local/bin:/usr/bin"), |p| {
            p == Path::new("/usr/bin/google-chrome-stable")
        });
        assert_eq!(found, Some(PathBuf::from("/usr/bin/google-chrome-stable")));
    }

    #[test]
    fn candidates_are_tried_in_order() {
        let candidates = vec!["chromium".to_string(), "google-chrome-stable".to_string()];
        // Both exist; the first must win.
        let found = resolve_program_with(&candidates, Some("/usr/bin"), |_| true);
        assert_eq!(found, Some(PathBuf::from("/usr/bin/chromium")));
    }

    #[test]
    fn an_absolute_candidate_skips_the_path_search() {
        let candidates = vec!["/snap/bin/chromium".to_string()];
        let found = resolve_program_with(&candidates, Some("/usr/bin"), |p| {
            p == Path::new("/snap/bin/chromium")
        });
        assert_eq!(found, Some(PathBuf::from("/snap/bin/chromium")));
    }

    #[test]
    fn nothing_installed_resolves_to_nothing() {
        let candidates = vec!["chromium".to_string()];
        assert!(resolve_program_with(&candidates, Some("/usr/bin"), |_| false).is_none());
    }

    #[test]
    fn every_known_chromium_packaging_is_covered() {
        // Debian/Arch, Raspberry Pi OS and older Ubuntu, Google's own deb, snap.
        for expected in [
            "chromium",
            "chromium-browser",
            "google-chrome-stable",
            "/snap/bin/chromium",
        ] {
            assert!(CHROMIUM_PROGRAMS.contains(&expected), "missing {expected}");
        }
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
    fn app_environment_is_passed_through() {
        let mut config = app("r1", chromium("http://a"));
        config
            .env
            .insert("LIBVA_DRIVER_NAME".into(), "nvidia".into());
        config.env.insert("NVD_BACKEND".into(), "direct".into());
        let spec = build(&config, &context());
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "LIBVA_DRIVER_NAME" && v == "nvidia"));
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "NVD_BACKEND" && v == "direct"));
    }

    #[test]
    fn app_environment_overrides_the_preset() {
        // Later entries win when the command is built, so an operator can
        // correct a preset without patching Suede.
        let mut config = app(
            "r1",
            Launcher::FirefoxKiosk {
                uri: "http://a".into(),
                extra_args: vec![],
            },
        );
        config.env.insert("MOZ_ENABLE_WAYLAND".into(), "0".into());
        let spec = build(&config, &context());
        let last = spec
            .env
            .iter()
            .rfind(|(k, _)| k == "MOZ_ENABLE_WAYLAND")
            .unwrap();
        assert_eq!(last.1, "0");
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
