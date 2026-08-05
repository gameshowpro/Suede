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
    // Without this a page cannot make a sound until somebody clicks it, and
    // on an appliance nobody ever will: Chromium suspends the AudioContext
    // and every <audio> and <video> element until a "user gesture", so a
    // stream that plays perfectly on a desk is silent on the machine. There
    // is no gesture to wait for here, and the operator chose what the
    // machine runs when they configured it, which is the consent the policy
    // exists to obtain.
    "--autoplay-policy=no-user-gesture-required",
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
    /// Loopback base for the API, e.g. `http://127.0.0.1:9088/api/v1`.
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
            program,
        } => {
            let programs = program_list(program.as_deref(), CHROMIUM_PROGRAMS);
            // Where the profile can live depends on what is going to open it.
            // A confined snap may write anywhere in $HOME except a hidden
            // directory, and Suede's state directory is under `.local` - so
            // the profile has to move rather than the browser be refused.
            let profile_dir = match resolve_program(&programs) {
                Some(found) if is_snap(&found) => snap_profile_dir(&found, &app.id)
                    .unwrap_or_else(|| profile_dir_for(&context.profiles_root, &app.id)),
                _ => profile_dir_for(&context.profiles_root, &app.id),
            };
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
                programs,
                args,
                env,
                profile_dir: Some(profile_dir),
                wipe_profile: !app.persist_profile,
            }
        }
        Launcher::FirefoxKiosk {
            uri,
            extra_args,
            program,
        } => {
            let mut args: Vec<String> = FIREFOX_KIOSK_ARGS.iter().map(|a| a.to_string()).collect();
            args.extend(extra_args.iter().cloned());
            args.push(expand_uri(uri, &app.id, context));
            // Firefox needs telling to use Wayland rather than XWayland.
            env.push(("MOZ_ENABLE_WAYLAND".to_string(), "1".to_string()));

            LaunchSpec {
                programs: program_list(program.as_deref(), FIREFOX_PROGRAMS),
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

/// The candidates to search, or just the one the operator named.
fn program_list(override_program: Option<&str>, preset: &[&str]) -> Vec<String> {
    match override_program {
        Some(named) => vec![named.to_string()],
        None => preset.iter().map(|p| p.to_string()).collect(),
    }
}

/// A profile directory a confined snap can actually open.
///
/// Snap's `home` interface grants access to `$HOME` but excludes hidden
/// directories, which is precisely where Suede keeps its state. Measured:
/// a profile under `~/.local/state` fails to create `SingletonLock` and
/// Chromium aborts; the same profile under a visible path works. The snap's
/// own `common` directory is chosen over inventing a visible one, because it
/// belongs to that snap and goes away when it is removed.
fn snap_profile_dir(program: &Path, app_id: &str) -> Option<PathBuf> {
    let snap = program.file_name()?.to_str()?;
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("snap")
            .join(snap)
            .join("common")
            .join("suede-profiles")
            .join(app_id),
    )
}

/// Whether a resolved program is a snap.
///
/// This matters because a confined snap cannot write outside its own
/// directories, and Suede hands every browser a private `--user-data-dir`
/// under its state directory. Chromium's answer to being unable to create
/// `SingletonLock` there is to abort, so the app crash-loops with a message
/// about profile corruption that says nothing about confinement.
pub fn is_snap(path: &Path) -> bool {
    // The launcher in $PATH is a symlink into /snap; resolving it is what
    // distinguishes Ubuntu's `chromium` shim from Debian's real binary of
    // the same name.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved.starts_with("/snap/") || path.starts_with("/snap/")
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
    let mut snap_fallback = None;
    for candidate in candidates {
        let mut found = None;
        let path = Path::new(candidate);
        if path.is_absolute() || candidate.contains('/') {
            if exists(path) {
                found = Some(path.to_path_buf());
            }
        } else {
            for dir in path_var
                .unwrap_or_default()
                .split(':')
                .filter(|d| !d.is_empty())
            {
                let full = Path::new(dir).join(candidate);
                if exists(&full) {
                    found = Some(full);
                    break;
                }
            }
        }
        let Some(found) = found else { continue };
        // A snap is remembered but passed over: on Ubuntu, `chromium` is a
        // shim for one while `chromium-browser` and `google-chrome-stable`
        // are ordinary binaries that can use the profile directory. On
        // Debian the same name is a real package and is taken immediately.
        // Reordering the candidate list could not express that, since which
        // name is the snap depends on the distribution.
        if is_snap(&found) {
            snap_fallback.get_or_insert(found);
            continue;
        }
        return Some(found);
    }
    // Nothing else is installed. Better a browser that may fail loudly than
    // no browser at all, and the health check explains what to expect.
    snap_fallback
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
            api_base: "http://127.0.0.1:9088/api/v1".into(),
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
            program: None,
        }
    }

    #[test]
    fn a_snap_is_passed_over_when_a_real_binary_exists() {
        // Ubuntu's `chromium` is a shim for a confined snap that cannot
        // write to the profile directory Suede assigns; `chromium-browser`
        // beside it is an ordinary binary that can.
        let candidates: Vec<String> = ["chromium", "chromium-browser"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found = resolve_program_with(&candidates, Some("/snap/bin:/usr/bin"), |path| {
            matches!(
                path.to_str(),
                Some("/snap/bin/chromium") | Some("/usr/bin/chromium-browser")
            )
        });
        assert_eq!(found.unwrap(), PathBuf::from("/usr/bin/chromium-browser"));
    }

    #[test]
    fn a_snap_is_still_used_when_it_is_all_there_is() {
        // No browser at all is worse than one that fails loudly, and the
        // health check explains what will happen.
        let candidates: Vec<String> = ["chromium", "chromium-browser"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found = resolve_program_with(&candidates, Some("/snap/bin:/usr/bin"), |path| {
            path.to_str() == Some("/snap/bin/chromium")
        });
        assert_eq!(found.unwrap(), PathBuf::from("/snap/bin/chromium"));
    }

    #[test]
    fn a_plain_binary_is_taken_in_order() {
        // Debian's `chromium` is real, and must not be passed over.
        let candidates: Vec<String> = ["chromium", "chromium-browser"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found = resolve_program_with(&candidates, Some("/usr/bin"), |path| {
            path.starts_with("/usr/bin")
        });
        assert_eq!(found.unwrap(), PathBuf::from("/usr/bin/chromium"));
    }

    #[test]
    fn an_explicit_program_settles_the_search() {
        let spec = build(
            &app(
                "r1",
                Launcher::ChromiumKiosk {
                    uri: "http://example.com".into(),
                    show_fps_counter: false,
                    extra_args: vec![],
                    program: Some("/opt/google/chrome/chrome".into()),
                },
            ),
            &context(),
        );
        assert_eq!(spec.programs, vec!["/opt/google/chrome/chrome".to_string()]);
    }

    #[test]
    fn a_snap_profile_goes_where_a_snap_can_write() {
        // Not the state directory: that is under `.local`, and the snap
        // `home` interface excludes hidden directories, which is what made
        // Chromium abort on being unable to create SingletonLock.
        let dir = snap_profile_dir(Path::new("/snap/bin/chromium"), "renderer").unwrap();
        let shown = dir.display().to_string().replace('\\', "/");
        assert!(
            shown.ends_with("/snap/chromium/common/suede-profiles/renderer"),
            "{shown}"
        );
        assert!(!shown.contains("/.local/"), "must not be hidden: {shown}");
    }

    #[test]
    fn chromium_preset_is_kiosk_and_wayland() {
        let spec = build(&app("r1", chromium("http://example.com")), &context());
        assert_eq!(spec.programs[0], "chromium");
        assert!(spec.programs.iter().any(|p| p == "google-chrome-stable"));
        assert!(spec.args.iter().any(|a| a == "--kiosk"));
        assert!(spec.args.iter().any(|a| a == "--ozone-platform=wayland"));
        assert!(spec.args.iter().any(|a| a == "--password-store=basic"));
        // An appliance has nobody to click "play".
        assert!(spec
            .args
            .iter()
            .any(|a| a == "--autoplay-policy=no-user-gesture-required"));
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
                    program: None,
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
                    program: None,
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
                    program: None,
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
        assert!(uri.contains("hb=http://127.0.0.1:9088/api/v1/apps/renderer-3/heartbeat"));
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
                program: None,
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
