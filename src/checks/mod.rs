//! Environment health checks and their remediations.
//!
//! Suede verifies the environment it depends on rather than silently mutating
//! it. Failures carry a documentation link; only fixes within the session
//! user's power are offered, and none ever run implicitly.

pub mod config_block;

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use crate::audio::AudioMonitor;
use crate::config::BootstrapConfig;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventHub, ServerEvent};
use crate::model::{Check, CheckStatus, PackageVersion};
use crate::sway::SwayClient;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const UNIT_NAME: &str = "suede.service";

/// Check identifiers, also used as the `{id}` in the fix endpoint.
pub mod ids {
    pub const SWAY_SOCKET: &str = "sway-socket";
    pub const SWAY_VERSION: &str = "sway-version";
    pub const BROWSERS: &str = "browsers";
    pub const PIPEWIRE: &str = "pipewire";
    pub const SYSTEMD_UNIT: &str = "systemd-unit";
    pub const SWAY_CONFIG: &str = "sway-config";
    pub const STATE_DIR: &str = "state-dir";
}

pub struct CheckRunner {
    bootstrap: Arc<BootstrapConfig>,
    sway: Arc<dyn SwayClient>,
    audio: Arc<dyn AudioMonitor>,
    events: EventHub,
    results: RwLock<Vec<Check>>,
}

impl CheckRunner {
    pub fn new(
        bootstrap: Arc<BootstrapConfig>,
        sway: Arc<dyn SwayClient>,
        audio: Arc<dyn AudioMonitor>,
        events: EventHub,
    ) -> Self {
        Self {
            bootstrap,
            sway,
            audio,
            events,
            results: RwLock::new(Vec::new()),
        }
    }

    pub fn results(&self) -> Vec<Check> {
        self.results.read().unwrap().clone()
    }

    /// Run every check, publishing an event when the outcome changes.
    pub async fn run_all(&self) -> Vec<Check> {
        let checks = vec![
            self.check_sway_socket().await,
            self.check_sway_version().await,
            self.check_browsers().await,
            self.check_pipewire().await,
            self.check_systemd_unit().await,
            self.check_sway_config(),
            self.check_state_dir(),
        ];

        let changed = {
            let mut guard = self.results.write().unwrap();
            let changed = *guard != checks;
            if changed {
                *guard = checks.clone();
            }
            changed
        };
        if changed {
            self.events
                .publish(ServerEvent::ChecksChanged(checks.clone()));
        }
        checks
    }

    /// Versions of the packages Suede depends on, for `GET /system`.
    pub async fn package_versions(&self) -> Vec<PackageVersion> {
        let mut packages = Vec::new();
        for (name, program, arg) in [
            ("sway", "sway", "--version"),
            ("chromium", "chromium", "--version"),
            ("firefox", "firefox", "--version"),
            ("pipewire", "pipewire", "--version"),
        ] {
            let version = run(program, &[arg])
                .await
                .ok()
                .filter(|output| output.success)
                .map(|output| first_line(&output.stdout));
            packages.push(PackageVersion {
                name: name.to_string(),
                version,
            });
        }
        packages
    }

    // --- individual checks ------------------------------------------------

    async fn check_sway_socket(&self) -> Check {
        let connected = self.sway.is_connected();
        let socket = crate::sway::discover_socket();
        let (status, detail) = match (connected, &socket) {
            (true, Some(path)) => (
                CheckStatus::Pass,
                format!("connected via {}", path.display()),
            ),
            (false, Some(path)) => (
                CheckStatus::Warn,
                format!(
                    "socket {} exists but the event connection is down",
                    path.display()
                ),
            ),
            (_, None) => (
                CheckStatus::Fail,
                "no sway IPC socket found; is sway running in this session?".to_string(),
            ),
        };
        self.check(ids::SWAY_SOCKET, "Sway IPC reachable", status, detail, None)
    }

    async fn check_sway_version(&self) -> Check {
        match self.sway.get_version().await {
            Ok(version) => {
                let detail = if version.supports_tearing() {
                    format!("{} (tearing control available)", version.display())
                } else {
                    format!(
                        "{} — tearing control needs sway 1.10 or newer",
                        version.display()
                    )
                };
                let status = if version.supports_tearing() {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Warn
                };
                self.check(ids::SWAY_VERSION, "Sway version", status, detail, None)
            }
            Err(error) => self.check(
                ids::SWAY_VERSION,
                "Sway version",
                CheckStatus::Fail,
                format!("could not query sway: {error}"),
                None,
            ),
        }
    }

    /// Browsers must be *functional*, not merely present on `PATH`.
    async fn check_browsers(&self) -> Check {
        let mut working = Vec::new();
        let mut broken = Vec::new();

        for program in ["chromium", "firefox"] {
            match run(program, &["--version"]).await {
                Ok(output) if output.success => {
                    working.push(format!("{program} ({})", first_line(&output.stdout)))
                }
                Ok(output) => broken.push(format!(
                    "{program} exited {}: {}",
                    output.code.unwrap_or(-1),
                    first_line(&output.stderr)
                )),
                Err(_) => {}
            }
        }

        let (status, detail) = if working.is_empty() && broken.is_empty() {
            (
                CheckStatus::Fail,
                "neither chromium nor firefox is installed".to_string(),
            )
        } else if working.is_empty() {
            (
                CheckStatus::Fail,
                format!("no working browser: {}", broken.join("; ")),
            )
        } else if broken.is_empty() {
            (
                CheckStatus::Pass,
                format!("available: {}", working.join(", ")),
            )
        } else {
            (
                CheckStatus::Warn,
                format!(
                    "available: {}; problems: {}",
                    working.join(", "),
                    broken.join("; ")
                ),
            )
        };

        self.check(
            ids::BROWSERS,
            "Browsers usable",
            status,
            detail,
            Some("getting-started/#browsers"),
        )
    }

    async fn check_pipewire(&self) -> Check {
        // Sink enumeration and per-app routing both go through pipewire-pulse.
        let dump_works = self.audio.is_available() || self.audio.refresh().await.is_ok();
        let pulse_socket = crate::util::runtime_dir()
            .map(|dir| dir.join("pulse/native").exists())
            .unwrap_or(false);

        let (status, detail) = match (dump_works, pulse_socket) {
            (true, true) => (
                CheckStatus::Pass,
                format!(
                    "{} sinks visible; pipewire-pulse is serving",
                    self.audio.sinks().len()
                ),
            ),
            (true, false) => (
                CheckStatus::Warn,
                "pipewire is running but pipewire-pulse is not; audio routing will not work"
                    .to_string(),
            ),
            (false, _) => (
                CheckStatus::Fail,
                "pw-dump is unavailable; audio features are disabled".to_string(),
            ),
        };

        self.check(
            ids::PIPEWIRE,
            "PipeWire audio",
            status,
            detail,
            Some("getting-started/#audio"),
        )
    }

    async fn check_systemd_unit(&self) -> Check {
        let enabled = run("systemctl", &["--user", "is-enabled", UNIT_NAME])
            .await
            .map(|output| first_line(&output.stdout) == "enabled")
            .unwrap_or(false);

        if enabled {
            return self.check(
                ids::SYSTEMD_UNIT,
                "Service starts with the session",
                CheckStatus::Pass,
                format!("{UNIT_NAME} is enabled"),
                None,
            );
        }

        let mut check = self.check(
            ids::SYSTEMD_UNIT,
            "Service starts with the session",
            CheckStatus::Fail,
            format!("{UNIT_NAME} is not enabled, so Suede will not start after a reboot"),
            Some("getting-started/#service"),
        );
        check.fix_available = true;
        check.fix_description = Some(format!(
            "Write ~/.config/systemd/user/{UNIT_NAME} if absent, then enable it \
             against sway-session.target"
        ));
        check
    }

    fn check_sway_config(&self) -> Check {
        let path = sway_config_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();

        if config_block::has_block(&text) {
            return self.check(
                ids::SWAY_CONFIG,
                "Sway configuration prepared",
                CheckStatus::Pass,
                format!("managed block present in {}", path.display()),
                None,
            );
        }

        let mut check = self.check(
            ids::SWAY_CONFIG,
            "Sway configuration prepared",
            CheckStatus::Warn,
            format!(
                "{} has no Suede block, so the session environment may not reach systemd",
                path.display()
            ),
            Some("getting-started/#sway-configuration"),
        );
        check.fix_available = true;
        check.fix_description = Some(format!(
            "Append a marker-delimited block to {}. Content outside the markers is untouched.",
            path.display()
        ));
        check
    }

    fn check_state_dir(&self) -> Check {
        let dir = &self.bootstrap.state_dir;
        let writable = std::fs::create_dir_all(dir)
            .and_then(|_| {
                let probe = dir.join(".write-probe");
                std::fs::write(&probe, b"")?;
                std::fs::remove_file(&probe)
            })
            .is_ok();

        let (status, detail) = if writable {
            (CheckStatus::Pass, format!("{} is writable", dir.display()))
        } else {
            (
                CheckStatus::Fail,
                format!(
                    "{} is not writable; configuration cannot be saved",
                    dir.display()
                ),
            )
        };
        self.check(
            ids::STATE_DIR,
            "State directory writable",
            status,
            detail,
            None,
        )
    }

    fn check(
        &self,
        id: &str,
        title: &str,
        status: CheckStatus,
        detail: String,
        docs: Option<&str>,
    ) -> Check {
        Check {
            id: id.to_string(),
            title: title.to_string(),
            status,
            detail,
            docs_url: docs.map(|path| self.bootstrap.docs_url(path)),
            fix_available: false,
            fix_description: None,
        }
    }

    // --- remediations -----------------------------------------------------

    /// Run the remediation for `id`, returning what was done.
    pub async fn fix(&self, id: &str) -> ApiResult<String> {
        let outcome = match id {
            ids::SYSTEMD_UNIT => self.fix_systemd_unit().await?,
            ids::SWAY_CONFIG => self.fix_sway_config()?,
            other => {
                return Err(ApiError::NotFound(format!(
                    "no automated fix is available for check {other:?}"
                )))
            }
        };
        self.run_all().await;
        Ok(outcome)
    }

    async fn fix_systemd_unit(&self) -> ApiResult<String> {
        let mut steps = Vec::new();

        let packaged = std::path::Path::new("/usr/lib/systemd/user").join(UNIT_NAME);
        let user_unit = crate::util::config_dir()
            .parent()
            .map(|dir| dir.join("systemd/user"))
            .unwrap_or_default()
            .join(UNIT_NAME);

        if !packaged.exists() && !user_unit.exists() {
            let executable = std::env::current_exe()
                .map_err(|error| ApiError::Internal(format!("cannot locate suede: {error}")))?;
            let unit = unit_file(&executable.display().to_string());
            if let Some(parent) = user_unit.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ApiError::Internal(format!("cannot create {}: {error}", parent.display()))
                })?;
            }
            std::fs::write(&user_unit, unit).map_err(|error| {
                ApiError::Internal(format!("cannot write {}: {error}", user_unit.display()))
            })?;
            steps.push(format!("wrote {}", user_unit.display()));
        }

        run("systemctl", &["--user", "daemon-reload"])
            .await
            .map_err(|error| {
                ApiError::Internal(format!("systemctl daemon-reload failed: {error}"))
            })?;
        steps.push("reloaded the systemd user daemon".to_string());

        let output = run("systemctl", &["--user", "enable", UNIT_NAME])
            .await
            .map_err(|error| ApiError::Internal(format!("systemctl enable failed: {error}")))?;
        if !output.success {
            return Err(ApiError::Internal(format!(
                "systemctl enable {UNIT_NAME} failed: {}",
                first_line(&output.stderr)
            )));
        }
        steps.push(format!("enabled {UNIT_NAME}"));

        Ok(steps.join("; "))
    }

    fn fix_sway_config(&self) -> ApiResult<String> {
        let path = sway_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ApiError::Internal(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();

        // Keep a copy: this file belongs to the user, not to Suede.
        if !existing.is_empty() {
            let backup = path.with_extension("suede-backup");
            let _ = std::fs::write(&backup, &existing);
        }

        let updated = config_block::upsert_block(&existing, config_block::SWAY_BLOCK_BODY);
        std::fs::write(&path, updated).map_err(|error| {
            ApiError::Internal(format!("cannot write {}: {error}", path.display()))
        })?;

        Ok(format!(
            "updated the Suede block in {}; reload sway to apply",
            path.display()
        ))
    }
}

fn sway_config_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".config")
        });
    base.join("sway/config")
}

fn unit_file(executable: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Suede display appliance daemon\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={executable} run\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=sway-session.target\n"
    )
}

struct CommandOutput {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run a program with a timeout, so a hung tool cannot stall the checks.
async fn run(program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
    let future = tokio::process::Command::new(program).args(args).output();

    let output = tokio::time::timeout(COMMAND_TIMEOUT, future)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"))??;

    Ok(CommandOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::MockAudio;
    use crate::sway::mock::MockSway;

    fn runner(state_dir: std::path::PathBuf) -> CheckRunner {
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir,
            ..BootstrapConfig::default()
        });
        CheckRunner::new(
            bootstrap,
            Arc::new(MockSway::with_fixtures()),
            Arc::new(MockAudio::with_sinks()),
            EventHub::new(),
        )
    }

    #[tokio::test]
    async fn every_check_reports_something() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        assert_eq!(checks.len(), 7);
        for id in [
            ids::SWAY_SOCKET,
            ids::SWAY_VERSION,
            ids::BROWSERS,
            ids::PIPEWIRE,
            ids::SYSTEMD_UNIT,
            ids::SWAY_CONFIG,
            ids::STATE_DIR,
        ] {
            assert!(checks.iter().any(|check| check.id == id), "missing {id}");
        }
    }

    #[tokio::test]
    async fn writable_state_directory_passes() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        let check = checks.iter().find(|c| c.id == ids::STATE_DIR).unwrap();
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn version_check_passes_on_a_recent_sway() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        let check = checks.iter().find(|c| c.id == ids::SWAY_VERSION).unwrap();
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn version_check_warns_on_an_older_sway() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::empty());
        sway.set_version(crate::sway::SwayVersion {
            major: 1,
            minor: 8,
            patch: 0,
            human_readable: None,
        });
        let runner = CheckRunner::new(
            bootstrap,
            sway,
            Arc::new(MockAudio::default()),
            EventHub::new(),
        );
        let checks = runner.run_all().await;
        let check = checks.iter().find(|c| c.id == ids::SWAY_VERSION).unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("1.10"));
    }

    #[tokio::test]
    async fn failing_checks_carry_documentation_links() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        // Browsers are absent in CI, so this check fails and must be actionable.
        let browsers = checks.iter().find(|c| c.id == ids::BROWSERS).unwrap();
        if browsers.status != CheckStatus::Pass {
            assert!(browsers.docs_url.is_some());
        }
    }

    #[tokio::test]
    async fn fixes_are_offered_only_where_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let checks = runner(dir.path().to_path_buf()).run_all().await;
        for check in &checks {
            if check.fix_available {
                assert!(
                    check.fix_description.is_some(),
                    "{} offers a fix without describing it",
                    check.id
                );
            }
        }
        // Package installation needs root and is never offered.
        let browsers = checks.iter().find(|c| c.id == ids::BROWSERS).unwrap();
        assert!(!browsers.fix_available);
    }

    #[tokio::test]
    async fn unknown_fix_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let error = runner(dir.path().to_path_buf())
            .fix("no-such-check")
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::NotFound(_)));
    }

    #[tokio::test]
    async fn checks_publish_only_when_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path().to_path_buf());
        let mut receiver = runner.events.subscribe();

        runner.run_all().await;
        assert!(receiver.try_recv().is_ok(), "first run should publish");

        runner.run_all().await;
        assert!(
            receiver.try_recv().is_err(),
            "an unchanged second run should stay quiet"
        );
    }

    #[test]
    fn generated_unit_starts_with_the_session() {
        let unit = unit_file("/usr/bin/suede");
        assert!(unit.contains("ExecStart=/usr/bin/suede run"));
        assert!(unit.contains("WantedBy=sway-session.target"));
    }
}
