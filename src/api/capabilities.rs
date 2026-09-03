//! Measuring what a browser can actually do, from inside it.
//!
//! The `video-decode` health check inspects drivers from the outside and can
//! only ever end in "confirm with a real page": whether Chromium will
//! hardware-decode on this GPU is known to nobody but Chromium, on this
//! machine, launched the way it would really be launched. So that is what
//! this does — spawn the operator's exact browser configuration against a
//! test page served by the daemon, let the page ask the media APIs, and have
//! it post the answers back. The operator sees measurements, not inference.
//!
//! Two ways in, one measurement. `POST /api/v1/apps/capabilities` holds the
//! connection while the browser starts, the page reports, and the browser is
//! terminated again. [`boot_measure`] runs the same thing once at startup —
//! but only when the stored measurement no longer describes this machine,
//! so most boots open no window at all. Either way the result lands in the
//! [`CapabilityStore`] and the `decode-measured` health check judges it.
//!
//! The page authenticates its report with the one-time id in its URL — it
//! cannot hold the API token, exactly like heartbeats — and both page and
//! report are accepted from the local machine only.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use tokio::sync::oneshot;

use super::ApiState;
use crate::api::json::Json;
use crate::capabilities::{MeasurementKey, StoredMeasurement};
use crate::error::{ApiError, ApiResult};
use crate::model::{AppConfig, Launcher};
use crate::supervisor::launcher;

pub use crate::model::{CapabilityReport, CodecSupport};

/// The page the launched browser is pointed at.
const PAGE: &str = include_str!("ui/capability-check.html");

/// The app id the check runs under. Distinct from every real app id so the
/// browser gets its own profile directory: Chromium sharing a profile with a
/// running instance silently delegates to it instead of starting, and the
/// running show is the last thing this should touch.
const CHECK_ID: &str = "capability-check";

/// How long the browser gets after exiting for a report already in flight.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// SIGTERM to SIGKILL escalation, matching the supervisor's manner.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the boot-time measurement waits: longer than the button's
/// default, because a cold browser on a small machine is slowest at boot.
const BOOT_TIMEOUT: Duration = Duration::from_secs(45);

/// How a capability check ended.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityOutcome {
    /// Whether the page reported before the browser was taken down.
    pub completed: bool,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<CapabilityReport>,
    /// When not completed: what happened instead, including the browser's
    /// last words to stderr where it left any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityQuery {
    /// How long to wait for the page's report. Clamped to 5–120, default 30.
    pub timeout_seconds: Option<u64>,
}

/// At most one check at a time: each one opens a window on the appliance's
/// displays, and two browsers fighting over them helps nobody.
#[derive(Default)]
pub struct CapabilityChecks {
    pending: Mutex<Option<Pending>>,
}

struct Pending {
    id: String,
    sender: Option<oneshot::Sender<CapabilityReport>>,
}

impl CapabilityChecks {
    /// Claim the slot. `None` when a check is already running.
    fn begin(&self) -> Option<(String, oneshot::Receiver<CapabilityReport>)> {
        let mut guard = self.pending.lock().unwrap();
        if guard.is_some() {
            return None;
        }
        let mut bytes = [0u8; 16];
        // The id is the page's whole authority to report, so it must not be
        // guessable by other local processes.
        getrandom::fill(&mut bytes).expect("the OS entropy source is unavailable");
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let (sender, receiver) = oneshot::channel();
        *guard = Some(Pending {
            id: id.clone(),
            sender: Some(sender),
        });
        Some((id, receiver))
    }

    /// Whether `id` names the check currently waiting.
    fn is_pending(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pending| pending.id == id)
    }

    /// Take the report channel for `id`, so exactly one report can land.
    fn take_sender(&self, id: &str) -> Option<oneshot::Sender<CapabilityReport>> {
        let mut guard = self.pending.lock().unwrap();
        match guard.as_mut() {
            Some(pending) if pending.id == id => pending.sender.take(),
            _ => None,
        }
    }

    /// Release the slot, whatever happened.
    fn finish(&self, id: &str) {
        let mut guard = self.pending.lock().unwrap();
        if guard.as_ref().is_some_and(|pending| pending.id == id) {
            *guard = None;
        }
    }

    /// The waiting check's id, if any. Tests drive the page's side with this.
    #[cfg(test)]
    pub fn pending_id(&self) -> Option<String> {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .map(|pending| pending.id.clone())
    }
}

/// Releases the slot however the measurement leaves — early error or done.
struct Slot {
    checks: std::sync::Arc<CapabilityChecks>,
    id: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.checks.finish(&self.id);
    }
}

/// The measurement itself, shared by the API handler and the boot check.
///
/// Launches `app`'s configuration against the test page, waits for the
/// page's report, terminates the browser, and remembers the outcome in the
/// [`crate::capabilities::CapabilityStore`] so the `decode-measured` health
/// check reflects the newest truth from either path.
pub async fn measure(
    state: &ApiState,
    app: &AppConfig,
    timeout: Duration,
) -> ApiResult<CapabilityOutcome> {
    if matches!(app.launcher, Launcher::Exec { .. }) {
        return Err(ApiError::BadRequest(
            "a capability check needs a browser launcher; an exec application \
             declares no page to test"
                .into(),
        ));
    }

    let Some((id, mut receiver)) = state.capabilities.begin() else {
        return Err(ApiError::Conflict(
            "a capability check is already running; wait for it to finish".into(),
        ));
    };
    let _slot = Slot {
        checks: state.capabilities.clone(),
        id: id.clone(),
    };

    let context = state.supervisor.launch_context();
    let check_app = check_variant(app, context, &id);
    let chosen = launcher::choose_program(&check_app);
    let Some(chosen) = chosen else {
        return Err(ApiError::BadRequest(format!(
            "none of these programs is installed: {}",
            launcher::candidates_for(&check_app).join(", ")
        )));
    };
    let key = MeasurementKey::new(app, &chosen);
    let spec = launcher::build(&check_app, context, Some(&chosen));

    // Its own profile, started clean every time — including for Firefox,
    // which the preset leaves profileless and which would otherwise hand the
    // URL to an already-running instance instead of starting.
    for profile in spec
        .profile_dir
        .iter()
        .cloned()
        .chain(firefox_profile(&check_app, context))
    {
        let _ = tokio::fs::remove_dir_all(&profile).await;
        tokio::fs::create_dir_all(&profile)
            .await
            .map_err(|error| ApiError::Internal(format!("cannot prepare {profile:?}: {error}")))?;
    }

    let log_path = context.log_path(CHECK_ID);
    if let Some(parent) = log_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let log = std::fs::File::create(&log_path)
        .map_err(|error| ApiError::Internal(format!("cannot open the check log: {error}")))?;

    let program = launcher::resolve_program(&spec.programs).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "none of these programs is installed: {}",
            spec.programs.join(", ")
        ))
    })?;

    let mut command = tokio::process::Command::new(&program);
    command
        .args(&spec.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log))
        .kill_on_drop(true);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| ApiError::Internal(format!("cannot launch {program:?}: {error}")))?;
    let started = Instant::now();
    tracing::info!(program = %program.display(), pid = ?child.id(), "capability check launched");

    let outcome = tokio::select! {
        report = &mut receiver => finished(report.ok(), started),
        _ = tokio::time::sleep(timeout) => CapabilityOutcome {
            completed: false,
            elapsed_ms: started.elapsed().as_millis() as u64,
            report: None,
            note: Some(join_note(
                format!("no report arrived within {}s", timeout.as_secs()),
                &log_path,
            )),
        },
        status = child.wait() => {
            // The report may already be in flight — a kiosk page has nothing
            // left to live for once it has posted, and some browsers exit.
            match tokio::time::timeout(EXIT_GRACE, &mut receiver).await {
                Ok(Ok(report)) => finished(Some(report), started),
                _ => CapabilityOutcome {
                    completed: false,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    report: None,
                    note: Some(join_note(
                        format!(
                            "the browser exited ({}) before reporting",
                            describe_exit(status.ok().and_then(|s| s.code()))
                        ),
                        &log_path,
                    )),
                },
            }
        }
    };

    terminate(child).await;

    // Remembered either way: a report is the newest truth, and a failure is
    // a fact the health check should state rather than paper over.
    state.capability_store.record(StoredMeasurement {
        measured_at: crate::util::unix_now(),
        app_id: app.id.clone(),
        key,
        report: outcome.report.clone(),
        note: outcome.note.clone(),
    });

    Ok(outcome)
}

/// The startup measurement: once, and only when the world changed.
///
/// Runs after the server starts accepting (the launched browser fetches its
/// page from the daemon), concurrently with the reconciler bringing the show
/// up — at boot the check's brief window is lost in the boot noise, which is
/// the one moment that is true. A stored measurement whose key still
/// describes this machine short-circuits the whole thing: no window opens.
pub async fn boot_measure(state: ApiState) {
    let desired = state.store.effective();
    if !desired.settings.measure_capabilities_on_start {
        return;
    }
    let Some(app) = crate::capabilities::subject(&desired) else {
        tracing::debug!("no browser application configured; capabilities not measured");
        return;
    };
    let Some(program) = launcher::choose_program(&app) else {
        tracing::debug!("no browser installed; capabilities not measured");
        return;
    };
    if state
        .capability_store
        .is_current(&MeasurementKey::new(&app, &program))
    {
        tracing::debug!("capability measurement is current; not re-measuring");
        // The check still needs to reflect the stored report on this boot.
        state.checks.run_all().await;
        return;
    }

    // The browser can only fetch its page once the daemon answers; poll our
    // own health endpoint rather than guessing at server startup timing.
    let base = state.supervisor.launch_context().api_base.clone();
    let healthz = format!(
        "{}/healthz",
        base.trim_end_matches('/')
            .trim_end_matches("/api/v1")
            .trim_end_matches('/')
    );
    for _ in 0..50 {
        if crate::probe::status_of(&healthz, Duration::from_secs(1))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tracing::info!(app = %app.id, "measuring browser capabilities (configuration changed)");
    match measure(&state, &app, BOOT_TIMEOUT).await {
        Ok(outcome) if outcome.completed => {
            tracing::info!(elapsed_ms = outcome.elapsed_ms, "capabilities measured");
        }
        Ok(outcome) => {
            tracing::warn!(note = ?outcome.note, "capability measurement did not complete");
        }
        Err(error) => {
            tracing::warn!(%error, "capability measurement could not run");
        }
    }
    // Whatever happened, the decode-measured check now has something to say.
    state.checks.run_all().await;
}

#[utoipa::path(
    post, path = "/api/v1/apps/capabilities", tag = "apps",
    request_body = AppConfig,
    params(CapabilityQuery),
    responses(
        (status = 200,
         description = "The check ran; `completed` says whether the page \
                        reported before the browser was taken down. A window \
                        opens on the appliance's displays while it runs.",
         body = CapabilityOutcome),
        (status = 400, description = "The launcher is not a browser, or no \
                        browser is installed"),
        (status = 409, description = "A capability check is already running"),
    )
)]
pub async fn run_capability_check(
    State(state): State<ApiState>,
    Query(query): Query<CapabilityQuery>,
    Json(app): Json<AppConfig>,
) -> ApiResult<Json<CapabilityOutcome>> {
    let timeout = Duration::from_secs(query.timeout_seconds.unwrap_or(30).clamp(5, 120));
    measure(&state, &app, timeout).await.map(Json)
}

/// The stored measurement, as `GET /apps/capabilities/last` serves it.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LastMeasurement {
    /// Unix seconds.
    pub measured_at: u64,
    /// Which application's configuration was measured.
    pub app_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<CapabilityReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Whether the measurement still describes this machine: same launcher,
    /// same browser binary, same driver, same daemon. `false` means the
    /// startup check will re-measure on the next boot.
    pub current: bool,
}

#[utoipa::path(
    get, path = "/api/v1/apps/capabilities/last", tag = "apps",
    responses(
        (status = 200, description = "The most recent capability measurement",
         body = LastMeasurement),
        (status = 404, description = "Nothing has been measured yet"),
    )
)]
pub async fn get_last(State(state): State<ApiState>) -> ApiResult<Json<LastMeasurement>> {
    let Some(stored) = state.capability_store.latest() else {
        return Err(ApiError::NotFound("nothing has been measured yet".into()));
    };
    let current = crate::capabilities::subject(&state.store.effective())
        .and_then(|app| launcher::choose_program(&app).map(|p| MeasurementKey::new(&app, &p)))
        .is_some_and(|key| key == stored.key);
    Ok(Json(LastMeasurement {
        measured_at: stored.measured_at,
        app_id: stored.app_id,
        report: stored.report,
        note: stored.note,
        current,
    }))
}

/// The served test page. Public in token mode — the browser cannot hold the
/// token — but only for the machine's own browser, and only while its check
/// is the one waiting.
pub async fn page(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> ApiResult<Html<&'static str>> {
    if !peer.ip().is_loopback() {
        return Err(ApiError::Forbidden(
            "the capability page is only served to the local machine".into(),
        ));
    }
    if !state.capabilities.is_pending(&id) {
        return Err(ApiError::NotFound(
            "no capability check with that id is waiting".into(),
        ));
    }
    Ok(Html(PAGE))
}

#[utoipa::path(
    post, path = "/api/v1/capability-check/{id}/result", tag = "apps",
    params(("id" = String, Path, description = "The check id from the page's own URL")),
    request_body = CapabilityReport,
    responses(
        (status = 204, description = "Report accepted"),
        (status = 403, description = "Only the local machine may report"),
        (status = 404, description = "No check with that id is waiting"),
    )
)]
pub async fn post_result(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    Json(report): Json<CapabilityReport>,
) -> ApiResult<StatusCode> {
    // Same posture as heartbeats: page content cannot hold the API token, so
    // the endpoint is open — to the machine the browser runs on, nobody else.
    if !peer.ip().is_loopback() {
        return Err(ApiError::Forbidden(
            "capability reports are only accepted from the local machine".into(),
        ));
    }
    let Some(sender) = state.capabilities.take_sender(&id) else {
        return Err(ApiError::NotFound(
            "no capability check with that id is waiting".into(),
        ));
    };
    // A dropped receiver means the check timed out in the same instant; the
    // page's answer is simply too late, which is not the page's problem.
    let _ = sender.send(report);
    Ok(StatusCode::NO_CONTENT)
}

/// The operator's application, redirected at the test page.
///
/// Everything else — browser choice, arguments, environment — is kept, which
/// is the point: the measurement is only honest if it runs the way the real
/// app would.
fn check_variant(app: &AppConfig, context: &launcher::LaunchContext, id: &str) -> AppConfig {
    let mut check = app.clone();
    check.id = CHECK_ID.to_string();
    check.persist_profile = false;
    // The page is served by the daemon itself; nothing to wait for, nothing
    // to watch.
    check.readiness = None;
    check.heartbeat = None;

    let url = format!("{}/capability-check/{id}", api_root(&context.api_base));
    match &mut check.launcher {
        Launcher::ChromiumKiosk { uri, .. } => *uri = url,
        Launcher::FirefoxKiosk {
            uri, extra_args, ..
        } => {
            *uri = url;
            // Firefox hands the URL to a running instance unless told it is
            // its own instance with its own profile.
            extra_args.push("--new-instance".into());
            extra_args.push("-profile".into());
            extra_args.push(context.profiles_root.join(CHECK_ID).display().to_string());
        }
        Launcher::Exec { .. } => unreachable!("rejected before this point"),
    }
    check
}

/// The Firefox profile directory the check passes, when it is a Firefox app.
fn firefox_profile(app: &AppConfig, context: &launcher::LaunchContext) -> Option<PathBuf> {
    matches!(app.launcher, Launcher::FirefoxKiosk { .. })
        .then(|| context.profiles_root.join(CHECK_ID))
}

/// `http://127.0.0.1:9088/api/v1` → `http://127.0.0.1:9088`.
fn api_root(api_base: &str) -> &str {
    let base = api_base.trim_end_matches('/');
    base.strip_suffix("/api/v1").unwrap_or(base)
}

fn finished(report: Option<CapabilityReport>, started: Instant) -> CapabilityOutcome {
    CapabilityOutcome {
        completed: report.is_some(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        report,
        note: None,
    }
}

fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(0) => "cleanly".to_string(),
        Some(code) => format!("status {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// The failure, and the browser's last stderr line when it left one.
fn join_note(what: String, log_path: &std::path::Path) -> String {
    match crate::supervisor::last_stderr_line(log_path) {
        Some(line) => format!("{what}; the browser's last words: {line}"),
        None => what,
    }
}

/// SIGTERM to the group, SIGKILL if it lingers — the supervisor's manner.
async fn terminate(mut child: tokio::process::Child) {
    let Some(pid) = child.id() else {
        // Already reaped by `wait` in the select arm.
        return;
    };
    crate::supervisor::signal_group(pid, crate::supervisor::TERM_SIGNAL);
    if tokio::time::timeout(TERMINATE_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        crate::supervisor::signal_group(pid, crate::supervisor::KILL_SIGNAL);
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support::{harness, Harness};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn kiosk(program: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "renderer",
            "launcher": {
                "kind": "chromium-kiosk",
                "uri": "http://example.test/",
                "program": program,
            },
        })
    }

    async fn send(harness: &Harness, request: Request<Body>, peer: &str) -> (StatusCode, Vec<u8>) {
        let mut request = request;
        let address: std::net::SocketAddr = peer.parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(address));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn report() -> serde_json::Value {
        serde_json::json!({
            "userAgent": "test",
            "gpuVendor": "NVIDIA",
            "gpuRenderer": "RTX A2000",
            "webgpu": true,
            "videoDecoderApi": true,
            "notes": [],
            "codecs": [{
                "label": "H.264 High 1080p60",
                "contentType": "video/mp4; codecs=\"avc1.64002A\"",
                "supported": true,
                "smooth": true,
                "powerEfficient": true,
                "hardware": true,
            }],
        })
    }

    #[tokio::test]
    async fn an_exec_app_is_refused() {
        let harness = harness(None);
        let body = serde_json::json!({
            "id": "helper",
            "launcher": { "kind": "exec", "command": "true", "args": [] },
        });
        let (status, _) = send(
            &harness,
            post_json("/api/v1/apps/capabilities", &body),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_missing_browser_is_named() {
        let harness = harness(None);
        let (status, body) = send(
            &harness,
            post_json(
                "/api/v1/apps/capabilities",
                &kiosk("/definitely/not/installed"),
            ),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("/definitely/not/installed"));
    }

    #[tokio::test]
    async fn only_one_check_runs_at_a_time() {
        let harness = harness(None);
        let claimed = harness.state.capabilities.begin();
        assert!(claimed.is_some());
        assert!(harness.state.capabilities.begin().is_none());

        let (status, _) = send(
            &harness,
            post_json("/api/v1/apps/capabilities", &kiosk("/bin/true")),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_remote_report_is_refused() {
        let harness = harness(None);
        let (id, _receiver) = harness.state.capabilities.begin().unwrap();
        let (status, _) = send(
            &harness,
            post_json(&format!("/api/v1/capability-check/{id}/result"), &report()),
            "192.168.1.50:2",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_report_for_an_unknown_id_is_404() {
        let harness = harness(None);
        let (status, _) = send(
            &harness,
            post_json("/api/v1/capability-check/deadbeef/result", &report()),
            "127.0.0.1:2",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_page_is_local_and_gated_on_a_waiting_check() {
        let harness = harness(None);
        let (id, _receiver) = harness.state.capabilities.begin().unwrap();

        let page = |id: &str| {
            Request::builder()
                .uri(format!("/capability-check/{id}"))
                .body(Body::empty())
                .unwrap()
        };
        let (status, body) = send(&harness, page(&id), "127.0.0.1:3").await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("capability"));

        let (status, _) = send(&harness, page(&id), "192.168.1.50:3").await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _) = send(&harness, page("deadbeef"), "127.0.0.1:3").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The full journey, with `/bin/true` standing in for the browser: the
    /// check launches it, the "page" reports, the outcome carries the report
    /// — and the measurement is remembered for the health check.
    #[tokio::test]
    async fn a_report_completes_the_check_and_is_remembered() {
        let harness = harness(None);
        let state = harness.state.clone();
        let router = harness.router.clone();

        let check = tokio::spawn(async move {
            let mut request = post_json(
                "/api/v1/apps/capabilities?timeoutSeconds=30",
                &kiosk("/bin/true"),
            );
            let address: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(address));
            router.oneshot(request).await.unwrap()
        });

        // The id only exists once the handler has claimed the slot.
        let id = loop {
            if let Some(id) = state.capabilities.pending_id() {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        let (status, _) = send(
            &harness,
            post_json(&format!("/api/v1/capability-check/{id}/result"), &report()),
            "127.0.0.1:4",
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let response = check.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let outcome: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(outcome["completed"], true);
        assert_eq!(outcome["report"]["gpuRenderer"], "RTX A2000");
        assert_eq!(outcome["report"]["codecs"][0]["hardware"], true);

        // The slot is free again, and the store remembers.
        assert!(state.capabilities.pending_id().is_none());
        let stored = state.capability_store.latest().unwrap();
        assert_eq!(stored.app_id, "renderer");
        assert!(stored.report.is_some());

        // And the last-measurement endpoint serves it.
        let (status, body) = send(
            &harness,
            Request::builder()
                .uri("/api/v1/apps/capabilities/last")
                .body(Body::empty())
                .unwrap(),
            "127.0.0.1:5",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let last: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(last["appId"], "renderer");
        assert_eq!(last["report"]["codecs"][0]["hardware"], true);
    }

    /// A browser that dies without reporting is explained, not just timed
    /// out — and the failure is remembered too.
    #[tokio::test]
    async fn an_early_exit_is_reported_as_such() {
        let harness = harness(None);
        let (status, body) = send(
            &harness,
            post_json(
                "/api/v1/apps/capabilities?timeoutSeconds=30",
                &kiosk("/bin/true"),
            ),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let outcome: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(outcome["completed"], false);
        assert!(outcome["note"].as_str().unwrap().contains("exited"));
        assert!(harness.state.capabilities.pending_id().is_none());

        let stored = harness.state.capability_store.latest().unwrap();
        assert!(stored.report.is_none());
        assert!(stored.note.unwrap().contains("exited"));
    }

    #[tokio::test]
    async fn nothing_measured_yet_is_404() {
        let harness = harness(None);
        let (status, _) = send(
            &harness,
            Request::builder()
                .uri("/api/v1/apps/capabilities/last")
                .body(Body::empty())
                .unwrap(),
            "127.0.0.1:5",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_api_root_is_derived_not_assumed() {
        assert_eq!(
            api_root("http://127.0.0.1:9088/api/v1"),
            "http://127.0.0.1:9088"
        );
        assert_eq!(
            api_root("http://127.0.0.1:9088/api/v1/"),
            "http://127.0.0.1:9088"
        );
        assert_eq!(api_root("http://127.0.0.1:9088"), "http://127.0.0.1:9088");
    }

    #[test]
    fn the_check_keeps_the_operators_configuration() {
        let context = launcher::LaunchContext {
            profiles_root: "/tmp/profiles".into(),
            log_root: "/tmp/logs".into(),
            api_base: "http://127.0.0.1:9088/api/v1".into(),
        };
        let app: AppConfig = serde_json::from_value(serde_json::json!({
            "id": "renderer",
            "launcher": {
                "kind": "chromium-kiosk",
                "uri": "http://example.test/",
                "extraArgs": ["--force-dark-mode"],
            },
            "env": { "LIBVA_DRIVER_NAME": "nvidia" },
        }))
        .unwrap();

        let check = check_variant(&app, &context, "abc123");
        assert_eq!(check.id, CHECK_ID, "its own profile, not the app's");
        match &check.launcher {
            Launcher::ChromiumKiosk {
                uri, extra_args, ..
            } => {
                assert_eq!(uri, "http://127.0.0.1:9088/capability-check/abc123");
                assert_eq!(extra_args, &["--force-dark-mode"], "operator args kept");
            }
            other => panic!("launcher changed kind: {other:?}"),
        }
        assert_eq!(check.env.get("LIBVA_DRIVER_NAME").unwrap(), "nvidia");
    }

    #[test]
    fn a_firefox_check_gets_its_own_instance_and_profile() {
        let context = launcher::LaunchContext {
            profiles_root: "/tmp/profiles".into(),
            log_root: "/tmp/logs".into(),
            api_base: "http://127.0.0.1:9088/api/v1".into(),
        };
        let app: AppConfig = serde_json::from_value(serde_json::json!({
            "id": "renderer",
            "launcher": { "kind": "firefox-kiosk", "uri": "http://example.test/" },
        }))
        .unwrap();

        let check = check_variant(&app, &context, "abc123");
        match &check.launcher {
            Launcher::FirefoxKiosk { extra_args, .. } => {
                assert!(extra_args.contains(&"--new-instance".to_string()));
                let position = extra_args.iter().position(|a| a == "-profile").unwrap();
                assert!(extra_args[position + 1].ends_with(CHECK_ID));
            }
            other => panic!("launcher changed kind: {other:?}"),
        }
    }
}
