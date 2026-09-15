//! Read-only endpoints, plus the imperative escape hatches.

use std::net::SocketAddr;

use crate::api::json::Json;
use async_trait::async_trait;
use axum::extract::{ConnectInfo, Path, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::{
    AudioSink, Check, Output, PowerVerb, ProjectionStats, Status, SystemInfo, Window,
};

#[utoipa::path(
    get, path = "/api/v1/outputs", tag = "observed",
    responses((status = 200, description = "Outputs reported by sway", body = Vec<Output>))
)]
pub async fn list_outputs(State(state): State<ApiState>) -> Json<Vec<Output>> {
    Json(state.snapshot.outputs())
}

#[utoipa::path(
    get, path = "/api/v1/ports", tag = "observed",
    responses((
        status = 200,
        description = "Every connector on the graphics hardware, attached or \
                       not. Offered so a client can let the operator configure \
                       a socket before its display arrives; sway remains the \
                       authority on what is actually driving a display.",
        body = Vec<crate::ports::Port>,
    ))
)]
pub async fn list_ports() -> Json<Vec<crate::ports::Port>> {
    Json(crate::ports::enumerate())
}

#[utoipa::path(
    get, path = "/api/v1/outputs/{name}", tag = "observed",
    params(("name" = String, Path, description = "Connector name, e.g. HDMI-A-1")),
    responses(
        (status = 200, description = "The output", body = Output),
        (status = 404, description = "No such output"),
    )
)]
pub async fn get_output(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Output>> {
    state
        .snapshot
        .output(&name)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no output named {name}")))
}

#[utoipa::path(
    get, path = "/api/v1/windows", tag = "observed",
    responses((status = 200, description = "Windows in sway's tree", body = Vec<Window>))
)]
pub async fn list_windows(State(state): State<ApiState>) -> Json<Vec<Window>> {
    Json(state.snapshot.windows())
}

#[utoipa::path(
    get, path = "/api/v1/audio/outputs", tag = "observed",
    responses((status = 200, description = "Audio sinks reported by PipeWire", body = Vec<AudioSink>))
)]
pub async fn list_audio_outputs(State(state): State<ApiState>) -> Json<Vec<AudioSink>> {
    Json(state.audio.sinks())
}

#[utoipa::path(
    get, path = "/api/v1/status", tag = "observed",
    responses((status = 200, description = "Reconciliation status", body = Status))
)]
pub async fn get_status(State(state): State<ApiState>) -> Json<Status> {
    Json(state.snapshot.status())
}

#[utoipa::path(
    get, path = "/api/v1/projection/stats", tag = "observed",
    responses((
        status = 200,
        description = "What the slicer measured over its last interval; \
                       null when no slicer is running",
        body = Option<ProjectionStats>,
    ))
)]
pub async fn get_projection_stats(State(state): State<ApiState>) -> Json<Option<ProjectionStats>> {
    Json(state.snapshot.projection_stats())
}

#[utoipa::path(
    get, path = "/api/v1/system", tag = "observed",
    responses((status = 200, description = "Daemon and environment information", body = SystemInfo))
)]
pub async fn get_system(State(state): State<ApiState>) -> Json<SystemInfo> {
    let version = state.sway.get_version().await.ok();
    Json(SystemInfo {
        suede_version: crate::VERSION.to_string(),
        build_id: crate::BUILD_ID.to_string(),
        sway_version: version.as_ref().map(|v| v.display()),
        hostname: hostname(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        packages: state.checks.package_versions().await,
        supports_tearing: version.as_ref().is_some_and(|v| v.supports_tearing()),
        web_ui_enabled: !state.bootstrap.auth_enabled(),
        power_verbs: state.bootstrap.power.clone(),
    })
}

#[utoipa::path(
    get, path = "/api/v1/system/checks", tag = "observed",
    responses((status = 200, description = "Environment health checks", body = Vec<Check>))
)]
pub async fn list_checks(State(state): State<ApiState>) -> Json<Vec<Check>> {
    Json(state.checks.run_all().await)
}

/// What a remediation did.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FixOutcome {
    pub id: String,
    pub detail: String,
}

#[utoipa::path(
    post, path = "/api/v1/system/checks/{id}/fix", tag = "observed",
    params(("id" = String, Path, description = "Check identifier")),
    responses(
        (status = 200, description = "What the fix did", body = FixOutcome),
        (status = 404, description = "No automated fix exists for this check"),
    )
)]
pub async fn fix_check(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<FixOutcome>> {
    let detail = state.checks.fix(&id).await?;
    tracing::info!(check = %id, %detail, "applied environment fix");
    Ok(Json(FixOutcome { id, detail }))
}

#[utoipa::path(
    post, path = "/api/v1/reconcile", tag = "control",
    responses((status = 200, description = "Status after the pass", body = Status))
)]
pub async fn reconcile_now(State(state): State<ApiState>) -> Json<Status> {
    Json(state.reconciler.reconcile().await)
}

/// A raw Sway command, for debugging.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SwayCommand {
    pub command: String,
}

#[utoipa::path(
    post, path = "/api/v1/sway/command", tag = "control",
    request_body = SwayCommand,
    responses(
        (status = 204, description = "Sway accepted the command"),
        (status = 503, description = "Sway rejected the command"),
    )
)]
pub async fn run_sway_command(
    State(state): State<ApiState>,
    Json(body): Json<SwayCommand>,
) -> ApiResult<axum::http::StatusCode> {
    // Used to be gated on `settings.allowRawSwayCommands`. The gate was
    // illusory: `Launcher::Exec` already runs "any executable, launched
    // verbatim", and apps are configured through this same API, so a client
    // that could flip the setting could already run anything by defining an
    // app. A flag that implies a protection it does not provide is worse
    // than no flag — see `Settings::allow_raw_sway_commands` for why the
    // field itself lingers a while longer.
    tracing::warn!(command = %body.command, "running raw sway command");
    state
        .sway
        .run_command(&body.command)
        .await
        .map_err(|error| ApiError::Unavailable(error.to_string()))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Carries out a permitted power verb. A trait, not a bare function call to
/// `systemctl`, so a test can prove the 403/400/permission checks never reach
/// the command step without spawning a real `systemctl` — which on a machine
/// that actually has systemd running is not a risk worth taking just to
/// cover a test in green.
#[async_trait]
pub trait PowerControl: Send + Sync + 'static {
    /// Run `verb`. `Err` carries what the operator should see: the first
    /// line of `systemctl`'s stderr, or a spawn failure's message.
    async fn run(&self, verb: PowerVerb) -> Result<(), String>;
}

/// The real implementation: shells out to `systemctl`.
pub struct SystemPower;

#[async_trait]
impl PowerControl for SystemPower {
    async fn run(&self, verb: PowerVerb) -> Result<(), String> {
        // Plainly, not `--user`: the daemon is a user service, and whether a
        // reboot or poweroff is actually allowed is logind's decision
        // (polkit), not something Suede can or should second-guess.
        match tokio::process::Command::new("systemctl")
            .arg(verb.as_str())
            .output()
            .await
        {
            Ok(output) if output.status.success() => Ok(()),
            // The interesting failure is a polkit refusal, and its text is
            // on stderr, not in the exit code.
            Ok(output) => Err(first_line(&output.stderr)),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("systemctl produced no output")
        .trim()
        .to_string()
}

/// Body of `POST /api/v1/system/power`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PowerRequest {
    pub verb: PowerVerb,
    /// The machine's hostname, repeated back. Proof that the caller meant
    /// this machine: a retry, a stray script or a fuzzer does not know it,
    /// and the cost of being wrong here is a dark video wall mid-show.
    pub confirm: String,
}

/// What `POST /api/v1/system/power` accepted.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PowerOutcome {
    pub verb: PowerVerb,
    pub detail: String,
}

#[utoipa::path(
    post, path = "/api/v1/system/power", tag = "control",
    request_body = PowerRequest,
    responses(
        (
            status = 202,
            description = "The instruction was accepted. Whether it completes \
                           is no longer observable over this connection.",
            body = PowerOutcome,
        ),
        (status = 400, description = "confirm does not repeat this machine's hostname"),
        (status = 403, description = "This verb is not permitted by bootstrap.power"),
        (status = 503, description = "systemctl refused, or could not be run"),
    )
)]
pub async fn power(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<PowerRequest>,
) -> ApiResult<(axum::http::StatusCode, Json<PowerOutcome>)> {
    if !state.bootstrap.power_allows(body.verb) {
        return Err(ApiError::Forbidden(format!(
            "{} is not permitted on this appliance; add it to `power` in suede.toml \
             (or SUEDE_POWER) and restart the daemon",
            body.verb.as_str()
        )));
    }

    // The expected hostname is not echoed: it is already available from
    // `GET /system`, and repeating it here would turn the check into a
    // formality rather than proof the caller looked it up.
    let matches_hostname = hostname()
        .is_some_and(|expected| body.confirm.trim().eq_ignore_ascii_case(expected.trim()));
    if !matches_hostname {
        return Err(ApiError::BadRequest(
            "confirm must repeat this machine's hostname, from GET /system".into(),
        ));
    }

    tracing::warn!(verb = body.verb.as_str(), %peer, "host power action requested");

    state
        .power
        .run(body.verb)
        .await
        .map_err(ApiError::Unavailable)?;

    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(PowerOutcome {
            verb: body.verb,
            detail: format!("{} accepted", body.verb.as_str()),
        }),
    ))
}

/// A [`PowerControl`] for tests: records every verb it was asked to run and
/// returns a configurable outcome, so a power-command test never depends on
/// `systemctl` existing — let alone succeeding — on whatever machine runs it.
#[cfg(test)]
pub(crate) mod power_mock {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::{PowerControl, PowerVerb};

    #[derive(Default)]
    pub struct MockPower {
        ran: Mutex<Vec<PowerVerb>>,
        fail: Mutex<Option<String>>,
    }

    impl MockPower {
        /// Every verb `run` was called with, in call order.
        pub fn ran(&self) -> Vec<PowerVerb> {
            self.ran.lock().unwrap().clone()
        }

        /// Make the next (and every subsequent) call fail with `message`, as
        /// a stand-in for a polkit refusal or a missing `systemctl`.
        pub fn fail_with(&self, message: &str) {
            *self.fail.lock().unwrap() = Some(message.to_string());
        }
    }

    #[async_trait]
    impl PowerControl for MockPower {
        async fn run(&self, verb: PowerVerb) -> Result<(), String> {
            self.ran.lock().unwrap().push(verb);
            match self.fail.lock().unwrap().clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::{harness, harness_with_power, Harness};
    use crate::model::PowerVerb;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn post_json(
        harness: &Harness,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        // `power`'s handler takes `ConnectInfo` directly (it logs the peer),
        // so a request built by hand needs it inserted the way the real
        // server's `into_make_service_with_connect_info` would.
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        let harness = harness(None);
        // Populate the snapshot the way a reconciliation pass would.
        harness.state.reconciler.refresh_outputs().await;
        harness.state.reconciler.refresh_windows().await;

        let response = harness
            .router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn lists_outputs() {
        let (status, body) = get_json("/api/v1/outputs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 3);
        assert_eq!(body[0]["name"], "HDMI-A-1");
        // camelCase is the API convention.
        assert!(body[0].get("currentMode").is_some());
    }

    #[tokio::test]
    async fn gets_one_output() {
        let (status, body) = get_json("/api/v1/outputs/HDMI-A-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "HDMI-A-1");
    }

    #[tokio::test]
    async fn unknown_output_is_404() {
        let (status, _) = get_json("/api/v1/outputs/HDMI-A-99").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lists_windows() {
        let (status, body) = get_json("/api/v1/windows").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn lists_audio_outputs() {
        let (status, body) = get_json("/api/v1/audio/outputs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reports_status() {
        let (status, body) = get_json("/api/v1/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "synced");
        assert!(body["divergences"].is_array());
    }

    #[tokio::test]
    async fn projection_stats_are_null_when_no_slicer_is_running() {
        // The harness never starts a slicer, so this is the "nothing to
        // report" case a client sees whenever projection is off or the
        // config has no seams.
        let (status, body) = get_json("/api/v1/projection/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_null());
    }

    #[tokio::test]
    async fn reports_system_information() {
        let (status, body) = get_json("/api/v1/system").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["suedeVersion"], crate::VERSION);
        assert_eq!(body["webUiEnabled"], true);
        assert!(body["packages"].is_array());
        // `power` is empty by default, and GET /system must say so.
        assert_eq!(body["powerVerbs"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn runs_health_checks() {
        let (status, body) = get_json("/api/v1/system/checks").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.as_array().unwrap().len(),
            crate::checks::ids::ALL.len()
        );
        assert!(body[0].get("fixAvailable").is_some());
    }

    #[tokio::test]
    async fn raw_sway_commands_run_unconditionally() {
        // The `allowRawSwayCommands` gate is gone: the endpoint always works.
        // The field survives on `Settings` only so an old saved document
        // still deserialises — see `Settings::allow_raw_sway_commands`.
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/sway/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"command":"reload"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(harness.sway.ran_command_containing("reload"));
    }

    // --- POST /system/power ------------------------------------------------

    #[tokio::test]
    async fn a_permitted_verb_with_the_right_confirm_reaches_the_command_step() {
        // Determinism: the command step goes through `harness.power`, a
        // `MockPower` that never touches a real `systemctl` — so this never
        // depends on whether one exists, or would refuse, on the machine
        // running the test.
        let harness = harness_with_power(None, &[PowerVerb::Reboot]);
        let host = super::hostname().expect("the test host must report a hostname");
        let body = format!(r#"{{"verb":"reboot","confirm":{host:?}}}"#);

        let (status, response) = post_json(&harness, "/api/v1/system/power", &body).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{response}");
        assert_eq!(response["verb"], "reboot");
        assert_eq!(harness.power.ran(), vec![PowerVerb::Reboot]);
    }

    #[tokio::test]
    async fn a_failing_command_is_reported_as_unavailable() {
        let harness = harness_with_power(None, &[PowerVerb::Poweroff]);
        harness
            .power
            .fail_with("Interactive authentication required.");
        let host = super::hostname().unwrap();
        let body = format!(r#"{{"verb":"poweroff","confirm":{host:?}}}"#);

        let (status, response) = post_json(&harness, "/api/v1/system/power", &body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
        assert_eq!(response["detail"], "Interactive authentication required.");
    }

    #[tokio::test]
    async fn a_verb_outside_bootstrap_power_is_forbidden_and_runs_nothing() {
        let harness = harness_with_power(None, &[PowerVerb::Poweroff]);
        let host = super::hostname().unwrap();
        let body = format!(r#"{{"verb":"reboot","confirm":{host:?}}}"#);

        let (status, response) = post_json(&harness, "/api/v1/system/power", &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{response}");
        assert_eq!(
            response["detail"],
            "reboot is not permitted on this appliance; add it to `power` in suede.toml \
             (or SUEDE_POWER) and restart the daemon"
        );
        assert!(harness.power.ran().is_empty());
    }

    #[tokio::test]
    async fn every_verb_is_forbidden_when_power_is_empty() {
        let harness = harness_with_power(None, &[]);
        let host = super::hostname().unwrap();
        for verb in ["reboot", "poweroff"] {
            let body = format!(r#"{{"verb":"{verb}","confirm":{host:?}}}"#);
            let (status, _) = post_json(&harness, "/api/v1/system/power", &body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{verb}");
        }
        assert!(harness.power.ran().is_empty());
    }

    #[tokio::test]
    async fn a_wrong_confirm_is_a_bad_request_and_runs_nothing() {
        let harness = harness_with_power(None, &[PowerVerb::Reboot]);
        let (status, response) = post_json(
            &harness,
            "/api/v1/system/power",
            r#"{"verb":"reboot","confirm":"not-this-machine"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert_eq!(
            response["detail"],
            "confirm must repeat this machine's hostname, from GET /system"
        );
        // The message must not hand back the value it refused to accept.
        assert!(!response["detail"]
            .as_str()
            .unwrap()
            .contains(&super::hostname().unwrap()));
        assert!(harness.power.ran().is_empty());
    }

    #[tokio::test]
    async fn get_system_reports_the_configured_verbs() {
        let harness = harness_with_power(None, &[PowerVerb::Reboot, PowerVerb::Poweroff]);
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["powerVerbs"],
            serde_json::json!(["reboot", "poweroff"])
        );
    }

    #[tokio::test]
    async fn unknown_check_fix_is_404() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/system/checks/nonsense/fix")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
