//! The HTTP API: REST for state, SSE for change notification.

pub mod apps;
pub mod capabilities;
pub mod config_routes;
pub mod docs;
pub mod events;
pub mod json;
pub mod observed;
#[cfg(test)]
mod profile_tests;
pub mod profiles;
pub mod projection;
pub mod ui;
pub mod wallpapers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::audio::AudioMonitor;
use crate::checks::CheckRunner;
use crate::config::BootstrapConfig;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventHub, ServerEvent};
use crate::model::{ConfigChange, DesiredState, OutputConfig};
use crate::reconciler::{ReconcileTrigger, Reconciler};
use crate::snapshot::Snapshot;
use crate::state::{ConditionalWriteError, StatePrecondition, StateStore, StateVersion};
use crate::supervisor::Supervisor;
use crate::sway::SwayClient;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct ApiState {
    pub bootstrap: Arc<BootstrapConfig>,
    pub store: Arc<StateStore>,
    pub snapshot: Arc<Snapshot>,
    pub events: EventHub,
    pub sway: Arc<dyn SwayClient>,
    pub audio: Arc<dyn AudioMonitor>,
    pub supervisor: Arc<Supervisor>,
    pub reconciler: Arc<Reconciler>,
    pub trigger: ReconcileTrigger,
    pub checks: Arc<CheckRunner>,
    pub wallpapers: Arc<crate::wallpapers::WallpaperStore>,
    pub profiles: Arc<crate::profiles::ProfileStore>,
    pub capabilities: Arc<capabilities::CapabilityChecks>,
    pub capability_store: Arc<crate::capabilities::CapabilityStore>,
    /// Runs `POST /system/power`'s permitted verb. A trait object, like
    /// [`SwayClient`] and [`AudioMonitor`], so tests never have to shell out
    /// to a real `systemctl`.
    pub power: Arc<dyn observed::PowerControl>,
    pub started_at: Instant,
}

impl ApiState {
    /// Persist a validated document, then reconcile.
    ///
    /// Writes return once *saved*, not once applied: application is
    /// asynchronous and may currently be impossible. `wait` opts into blocking
    /// until reconciliation settles.
    ///
    /// Unconditional: it folds the one shared working copy in (if a working
    /// copy is live, it — not the saved document — is the basis this commits
    /// on) rather than being refused; see [`StateStore::stage_replace_if`].
    pub async fn commit(
        &self,
        next: DesiredState,
        section: &str,
        wait: Option<u64>,
    ) -> ApiResult<DesiredState> {
        self.commit_if(StatePrecondition::default(), section, wait, move |_| {
            Ok(next)
        })
        .await
        .map(|(document, _)| document)
    }

    /// Commit a configuration transition only if the document identities the
    /// caller read still hold.
    ///
    /// Building, validation and the version comparison happen under the
    /// state-store write lock, so a competing preview or section write cannot
    /// slip between them. The capability status validation needs is read
    /// *before* that lock is taken, and the disk write happens *after* it is
    /// released, on a blocking thread: the store's lock covers the document
    /// and nothing else.
    pub async fn commit_if<F>(
        &self,
        expected: StatePrecondition,
        section: &str,
        wait: Option<u64>,
        prepare: F,
    ) -> ApiResult<(DesiredState, StateVersion)>
    where
        F: FnOnce(&DesiredState) -> ApiResult<DesiredState>,
    {
        let context = self.validation_context();
        let (saved, version) = self
            .store
            .stage_replace_if(expected, |basis| {
                let mut next = prepare(basis)?;
                validate_configuration_against(&mut next, basis, &context)?;
                // Structural, not a client convention: whatever `prepare` or
                // validation left here is discarded unconditionally before
                // this document can reach disk. See [`crate::model::TemporarySettings`].
                if let Some(projection) = next.projection.as_mut() {
                    projection.temporary = Default::default();
                }
                Ok(next)
            })
            .map_err(map_conditional_write_error)?;

        // The change is accepted and live from here on: it is published and
        // reconciled even if the save fails, because the alternative is a
        // daemon whose outputs and whose clients disagree about what the
        // document says. A failed save is reported to this caller as a 500
        // and stays visible in `/status` until one succeeds.
        let persisted = self.persist().await;

        self.publish_config_change(section, &saved, &version);

        match wait {
            Some(seconds) => {
                let timeout = std::time::Duration::from_secs(seconds.clamp(1, 120));
                if tokio::time::timeout(timeout, self.reconciler.reconcile())
                    .await
                    .is_err()
                {
                    tracing::warn!(section, "reconciliation did not settle within the wait");
                }
            }
            None => self.trigger.request("config write"),
        }

        persisted?;
        Ok((saved, version))
    }

    /// Like [`Self::commit_if`], but the document `prepare` returns is
    /// committed literally: `preserve_retained`'s calibration refill does
    /// not run.
    ///
    /// Used by profile activation and the geometry/canvas DELETE routes. An ordinary
    /// Simple-mode PUT that omits `geometry` or `canvas` means "leave this
    /// alone", so [`validate_configuration_against`] refills it from the
    /// previous document — and would silently undo an explicit clear made
    /// by the very same mechanism if the DELETE handlers' own writes went
    /// through it too. See [`crate::projection_policy::preserve_retained`].
    pub async fn commit_literal_if<F>(
        &self,
        expected: StatePrecondition,
        section: &str,
        wait: Option<u64>,
        prepare: F,
    ) -> ApiResult<(DesiredState, StateVersion)>
    where
        F: FnOnce(&DesiredState) -> ApiResult<DesiredState>,
    {
        let context = self.validation_context();
        let (saved, version) = self
            .store
            .stage_replace_if(expected, |basis| {
                let mut next = prepare(basis)?;
                validate_configuration_literal(&mut next, basis, &context)?;
                // Structural, not a client convention: see the matching
                // reset in `commit_if` above.
                if let Some(projection) = next.projection.as_mut() {
                    projection.temporary = Default::default();
                }
                Ok(next)
            })
            .map_err(map_conditional_write_error)?;

        let persisted = self.persist().await;

        self.publish_config_change(section, &saved, &version);

        match wait {
            Some(seconds) => {
                let timeout = std::time::Duration::from_secs(seconds.clamp(1, 120));
                if tokio::time::timeout(timeout, self.reconciler.reconcile())
                    .await
                    .is_err()
                {
                    tracing::warn!(section, "reconciliation did not settle within the wait");
                }
            }
            None => self.trigger.request("config write"),
        }

        persisted?;
        Ok((saved, version))
    }

    /// Run the state store's blocking disk write off the async executor.
    async fn persist(&self) -> ApiResult<()> {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.flush()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ApiError::Internal(format!(
                "the change is live but was not saved and will be lost on restart: {error}"
            ))),
            Err(error) => Err(ApiError::Internal(format!(
                "the change is live but the save did not run: {error}"
            ))),
        }
    }

    /// Apply an uncommitted working copy under the same atomic precondition
    /// contract used by saves. Every connected client — not only this one —
    /// learns of it through `config_changed`.
    pub fn preview_if(
        &self,
        expected: StatePrecondition,
        next: DesiredState,
    ) -> ApiResult<(DesiredState, StateVersion)> {
        self.preview_with_if(expected, move |_basis| Ok(next))
    }

    /// Like [`Self::preview_if`], but `prepare` computes the candidate
    /// document from the effective basis *inside* the store's write lock,
    /// rather than the caller reading `effective()` beforehand and handing
    /// over an already-built document. That closes the gap where another
    /// client's preview could land between reading the basis and staging the
    /// new one: whatever `prepare` sees is guaranteed to still be current
    /// when it runs. `preview_if` is the common case where the candidate
    /// does not depend on the basis and is just this call with a
    /// basis-ignoring closure.
    pub fn preview_with_if<F>(
        &self,
        expected: StatePrecondition,
        prepare: F,
    ) -> ApiResult<(DesiredState, StateVersion)>
    where
        F: FnOnce(&DesiredState) -> ApiResult<DesiredState>,
    {
        let context = self.validation_context();
        let mut projection_only = false;
        let result = self.store.set_preview_if(expected, |basis| {
            let mut next = prepare(basis)?;
            validate_configuration_against(&mut next, basis, &context)?;
            projection_only = projection_only_change(basis, &next);
            Ok(next)
        });
        let accepted = result.map_err(map_conditional_write_error)?;
        self.publish_config_change("all", &accepted.0, &accepted.1);
        if projection_only {
            self.trigger.request_projection();
        } else {
            self.trigger.request("working copy");
        }
        Ok(accepted)
    }

    /// Discard the working copy — anyone's — only when the caller named a
    /// version that still matches (a stale precondition still gets a 409); an
    /// unconditional call discards whatever is live. Published like every
    /// other change to the effective document.
    pub fn revert_if(
        &self,
        expected: StatePrecondition,
    ) -> ApiResult<(DesiredState, StateVersion)> {
        let accepted = self
            .store
            .clear_preview_if(expected)
            .map_err(map_revert_error)?;
        self.publish_config_change("all", &accepted.0, &accepted.1);
        self.trigger.request("revert");
        Ok(accepted)
    }

    /// Publish `config_changed` for a change to the effective document: a
    /// preview PUT, a commit, a revert, reconciler adoption, or load-time
    /// repair. `document` is the effective document that produced `version`
    /// — the working copy if one exists, else the saved one — and
    /// `committed` mirrors `document.committed`.
    pub fn publish_config_change(
        &self,
        section: &str,
        document: &DesiredState,
        version: &StateVersion,
    ) {
        self.events
            .publish(ServerEvent::ConfigChanged(Box::new(config_change(
                section, document, version,
            ))));
    }

    pub fn validate_configuration(&self, next: &mut DesiredState) -> ApiResult<()> {
        let context = self.validation_context();
        let previous = self.store.effective();
        validate_configuration_against(next, &previous, &context)
    }

    /// Read everything a validation needs from outside the state store.
    ///
    /// Called before the store is entered, never from inside a `prepare`
    /// closure: the closure runs under the document write lock, and taking a
    /// snapshot lock there is the nesting that deadlocked an appliance once.
    fn validation_context(&self) -> ValidationContext {
        ValidationContext::from_snapshot(&self.snapshot, self.bootstrap.allow_overlaps)
    }

    /// Reject a write whose `If-Match` revision is stale.
    pub fn check_precondition(&self, if_match: Option<&str>) -> ApiResult<()> {
        let Some(value) = if_match else {
            return Ok(());
        };
        let expected: u64 = value
            .trim()
            .trim_matches('"')
            .parse()
            .map_err(|_| ApiError::BadRequest("If-Match must be a revision number".into()))?;
        let current = self.store.revision();
        if expected != current {
            return Err(ApiError::Conflict(format!(
                "document is at revision {current}, not {expected}"
            )));
        }
        Ok(())
    }
}

/// The facts a document validation needs that do not live in the document.
///
/// Gathered in one place because they have to be gathered *early*: every
/// validation below runs inside a state-store transition, and nothing inside
/// one may take another lock. The reconciler builds one of these too, so an
/// adoption is validated exactly like a client's write.
pub struct ValidationContext {
    control: crate::model::ProjectionControlStatus,
    slicer_running: bool,
    allow_overlaps: bool,
}

impl ValidationContext {
    pub fn from_snapshot(snapshot: &Snapshot, allow_overlaps: bool) -> Self {
        Self {
            control: snapshot.projection_control(),
            slicer_running: snapshot.slicer_running(),
            allow_overlaps,
        }
    }
}

/// Validate `next` as a replacement for `previous`, the one document this
/// transition builds on.
///
/// Takes no lock of its own, so it is safe to call from inside a state-store
/// transition — which is where it has to run, or another client's write could
/// land between the check and the swap.
///
/// Before validating, `next` gets Simple's retention refill and, when the
/// projection mode changes, the Exact-mode switching rules
/// ([`crate::projection_policy::transition`]): entering Exact may reset
/// `next`'s geometry, so the document a caller gets back is the one stored.
pub fn validate_configuration_against(
    next: &mut DesiredState,
    previous: &DesiredState,
    context: &ValidationContext,
) -> ApiResult<()> {
    crate::projection_policy::preserve_retained(next, previous);
    crate::projection_policy::transition(next, previous, context.allow_overlaps).map_err(
        |errors| {
            ApiError::Validation(format!(
                "cannot reset to the exact-mode defaults: {}",
                errors.join("; ")
            ))
        },
    )?;
    validate_configuration_literal(next, previous, context)
}

/// As [`validate_configuration_against`], but without the retention refill:
/// `next` is validated and activated exactly as given. Used by
/// [`ApiState::commit_literal_if`] for writes — the geometry/canvas DELETE
/// routes — that must not have an omitted field resurrected from `previous`.
pub fn validate_configuration_literal(
    next: &mut DesiredState,
    previous: &DesiredState,
    context: &ValidationContext,
) -> ApiResult<()> {
    next.validate(context.allow_overlaps)
        .map_err(|errors| ApiError::Validation(errors.join("; ")))?;
    crate::projection_policy::validate_activation(
        next,
        previous,
        &context.control,
        context.slicer_running,
        context.allow_overlaps,
    )
    .map_err(ApiError::Validation)
}

/// Projection-only previews cannot change Sway output placement, apps, or
/// other settings. Classify under the same state lock as the accepted edit;
/// reading the old document outside that lock would race another operator.
fn projection_only_change(previous: &DesiredState, next: &DesiredState) -> bool {
    if previous.outputs.len() != next.outputs.len() {
        return false;
    }
    let mut other_fields = next.clone();
    other_fields.committed = previous.committed;
    other_fields.revision = previous.revision;
    other_fields.projection = previous.projection.clone();
    for (output, previous_output) in other_fields.outputs.iter_mut().zip(&previous.outputs) {
        output.geometry = previous_output.geometry.clone();
    }
    other_fields == *previous
}

pub fn map_conditional_write_error(error: ConditionalWriteError<ApiError>) -> ApiError {
    match error {
        ConditionalWriteError::Rejected(error) => error,
        ConditionalWriteError::Precondition { current } => stale_precondition(&current),
        ConditionalWriteError::State(error) => ApiError::Internal(error.to_string()),
    }
}

fn map_revert_error(error: ConditionalWriteError<std::convert::Infallible>) -> ApiError {
    match error {
        ConditionalWriteError::Rejected(never) => match never {},
        ConditionalWriteError::Precondition { current } => stale_precondition(&current),
        ConditionalWriteError::State(error) => ApiError::Internal(error.to_string()),
    }
}

/// A precondition was named — `If-Match`, `If-Config-Generation` or
/// `If-Config-Epoch` — and no longer matches. Only sent when the client
/// chose to name a version at all: an unconditional write is never rejected
/// this way.
fn stale_precondition(current: &StateVersion) -> ApiError {
    ApiError::Conflict(format!(
        "document is at revision {} and generation {}",
        current.revision, current.generation
    ))
}

/// Build the `config_changed` payload for a change to the effective
/// document. `document` is what `version` identifies — the working copy if
/// one exists, else the saved one — and `committed` mirrors
/// `document.committed`.
pub fn config_change(
    section: &str,
    document: &DesiredState,
    version: &StateVersion,
) -> ConfigChange {
    ConfigChange {
        revision: version.revision,
        generation: version.generation,
        epoch: version.epoch.clone(),
        committed: document.committed,
        section: section.to_string(),
        config: document.clone(),
    }
}

/// The output-raster lookup every arrangement handler shares.
///
/// Resolves, per output: the configured mode if there is one, else whatever
/// was adopted (`OutputConfig::effective_mode` already covers both), else
/// the observed output's current mode, matched by the output's own match
/// rule — the same lookup the reconciler and `api/observed.rs` use to pair a
/// configuration entry with what Sway reports. Returns unrotated mode
/// dimensions; the arrangement solver applies the 90/270 transform swap
/// itself, so this lookup is free of that concern.
pub fn raster_lookup(snapshot: &Snapshot) -> impl Fn(&OutputConfig) -> Option<(u32, u32)> {
    let observed = snapshot.outputs();
    move |output: &OutputConfig| {
        let mode = output.effective_mode().or_else(|| {
            observed
                .iter()
                .find(|candidate| output.r#match.matches(candidate))
                .and_then(|candidate| candidate.current_mode)
        })?;
        Some((
            u32::try_from(mode.width).ok()?,
            u32::try_from(mode.height).ok()?,
        ))
    }
}

/// Build the main listener's router: the whole API, the UI and the docs.
///
/// The three app-facing routes — heartbeats, the capability page and its
/// result — are authorized by the caller being on this machine. That holds
/// on a routable bind, where off-box callers arrive with their own address,
/// so they keep answering loopback callers here for content that hardcodes
/// the main port. On a loopback bind it proves nothing: every caller is
/// loopback, including whatever a reverse proxy on this machine forwards.
/// There they answer 404 naming the local listener ([`local_router`]),
/// which is what launched apps are given.
pub fn router(state: ApiState) -> Router {
    let app_facing_here = !state.bootstrap.bind.ip().is_loopback();
    let heartbeat = if app_facing_here {
        post(apps::heartbeat)
    } else {
        post(served_only_locally)
    };
    let capability_result = if app_facing_here {
        post(capabilities::post_result)
    } else {
        post(served_only_locally)
    };
    let capability_page = if app_facing_here {
        get(capabilities::page)
    } else {
        get(served_only_locally)
    };

    let api = Router::new()
        // Observed state.
        .route("/outputs", get(observed::list_outputs))
        .route("/outputs/{name}", get(observed::get_output))
        .route("/ports", get(observed::list_ports))
        .route("/windows", get(observed::list_windows))
        .route("/av", get(observed::list_av_devices))
        .route("/status", get(observed::get_status))
        .route("/projection/stats", get(observed::get_projection_stats))
        .route(
            "/projection/recommendation",
            get(projection::recommend_resolution),
        )
        .route("/projection/arrangement", get(projection::get_arrangement))
        .route("/system", get(observed::get_system))
        .route("/system/gpu", get(observed::get_gpu))
        .route("/system/pipeline", get(observed::get_pipeline))
        .route("/system/cpu", get(observed::get_cpu))
        .route("/system/browsers", get(observed::list_browsers))
        .route("/system/checks", get(observed::list_checks))
        .route("/system/checks/{id}/fix", post(observed::fix_check))
        .route("/system/power", post(observed::power))
        // Applications.
        .route("/apps", get(apps::list_apps))
        .route("/apps/preview", post(apps::preview_app))
        .route(
            "/apps/capabilities",
            post(capabilities::run_capability_check),
        )
        .route("/apps/capabilities/last", get(capabilities::get_last))
        .route(
            "/apps/capabilities/measure",
            post(capabilities::measure_subject),
        )
        .route("/capability-check/{id}/result", capability_result)
        .route("/apps/{id}/status", get(apps::get_app_status))
        .route("/apps/{id}/restart", post(apps::restart_app))
        .route("/apps/{id}/activate", post(apps::activate_app))
        .route("/apps/{id}/deactivate", post(apps::deactivate_app))
        .route("/apps/{id}/heartbeat", heartbeat.layer(heartbeat_cors()))
        // Desired state.
        .route(
            "/config",
            get(config_routes::get_config).put(config_routes::put_config),
        )
        .route(
            "/config/outputs",
            get(config_routes::get_outputs).put(config_routes::put_outputs),
        )
        .route("/config/outputs/{key}", get(config_routes::get_output))
        .route("/config/outputs/{key}", put(config_routes::put_output))
        .route(
            "/config/outputs/{key}",
            delete(config_routes::delete_output),
        )
        .route(
            "/config/outputs/{key}/geometry",
            delete(config_routes::delete_output_geometry),
        )
        .route(
            "/config/backgrounds",
            get(config_routes::get_backgrounds).put(config_routes::put_backgrounds),
        )
        .route(
            "/config/backgrounds/{id}",
            put(config_routes::put_background).delete(config_routes::delete_background),
        )
        .route(
            "/config/apps",
            get(config_routes::get_apps).put(config_routes::put_apps),
        )
        .route("/config/apps/{id}", get(config_routes::get_app))
        .route("/config/apps/{id}", put(config_routes::put_app))
        .route("/config/apps/{id}", delete(config_routes::delete_app))
        .route(
            "/config/settings",
            get(config_routes::get_settings).put(config_routes::put_settings),
        )
        .route(
            "/config/projection",
            get(config_routes::get_projection).put(config_routes::put_projection),
        )
        .route(
            "/config/projection/canvas",
            delete(config_routes::delete_projection_canvas),
        )
        .route(
            "/config/projection/arrangement",
            get(config_routes::get_projection_arrangement)
                .put(config_routes::put_projection_arrangement),
        )
        .route("/config/revert", post(config_routes::revert_config))
        .route("/profiles", get(profiles::list).post(profiles::create))
        .route(
            "/profiles/import",
            post(profiles::import).layer(axum::extract::DefaultBodyLimit::max(
                crate::profiles::MAX_PROFILE_BYTES,
            )),
        )
        .route(
            "/profiles/{id}",
            get(profiles::download)
                .put(profiles::update)
                .delete(profiles::delete),
        )
        .route("/profiles/{id}/activate", post(profiles::activate))
        // Imperative escape hatches.
        .route("/reconcile", post(observed::reconcile_now))
        .route("/sway/command", post(observed::run_sway_command))
        // Wallpapers.
        .route("/wallpapers", get(wallpapers::list))
        .route(
            "/wallpapers/{id}",
            get(wallpapers::download)
                .put(wallpapers::upload)
                .delete(wallpapers::delete),
        )
        // Uploads are images, well over axum's default 2 MB body limit.
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::wallpapers::MAX_BYTES,
        ))
        // Events.
        .route("/events", get(events::stream));

    let mut app = Router::new()
        .nest("/api/v1", api)
        .route("/healthz", get(health))
        // Mounted regardless of token mode: the appliance's own browser is
        // what fetches it, and that browser cannot hold the token.
        .route("/capability-check/{id}", capability_page)
        .route("/api-docs/openapi.json", get(docs::openapi_json));

    // The web UI and the Scalar docs page are served in token mode too. They
    // are static and embed no credential (see `is_static_page`); the UI's own
    // API calls succeed only when something in front of Suede, an
    // authenticating proxy, adds the token.
    app = app.merge(docs::scalar_router()).merge(ui::router());

    app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        authenticate,
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http())
    .with_state(state)
}

/// Build the local listener's router: the three app-facing routes and
/// nothing else.
///
/// The local listener is always bound to loopback (see
/// [`BootstrapConfig::local_bind`]), and is never what a reverse proxy
/// forwards to, so a loopback caller here really is page content on this
/// machine. The handlers keep their own loopback checks regardless. No
/// token is asked for — page content cannot hold one, which is why these
/// routes are authorized by locality in the first place — but the Host and
/// cross-site checks still apply, so a page elsewhere cannot use a browser
/// on this machine to post a capability result.
pub fn local_router(state: ApiState) -> Router {
    Router::new()
        .route(
            "/api/v1/apps/{id}/heartbeat",
            post(apps::heartbeat).layer(heartbeat_cors()),
        )
        .route(
            "/api/v1/capability-check/{id}/result",
            post(capabilities::post_result),
        )
        .route("/capability-check/{id}", get(capabilities::page))
        .fallback(not_on_the_local_listener)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard_local,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Heartbeats come from page content, which may be served from a different
/// origin or port than the appliance's own API (or loaded over `file://`),
/// so this one route answers preflight and carries `Access-Control-Allow-*`
/// on whichever listener serves it. The rest of the API stays same-origin
/// only; scoping the layer to this route (rather than the whole router)
/// keeps it that way.
fn heartbeat_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST])
        .allow_private_network(true)
}

/// An app-facing route asked of the main listener while it is bound to
/// loopback; see [`router`].
async fn served_only_locally(State(state): State<ApiState>) -> ApiError {
    ApiError::NotFound(format!(
        "this route is served only on the local listener while the API is bound to \
         loopback ({bind}): every caller of a loopback listener is local, a reverse proxy's \
         requests included, so being local proves nothing here. Use the api_base Suede \
         hands launched apps, {api_base}",
        bind = state.bootstrap.bind,
        api_base = state.supervisor.launch_context().api_base,
    ))
}

/// Anything but the three app-facing routes, asked of the local listener.
async fn not_on_the_local_listener(State(state): State<ApiState>) -> ApiError {
    ApiError::NotFound(format!(
        "this listener serves only app heartbeats and the capability check; the API, the \
         web UI and /healthz are on {bind}",
        bind = state.bootstrap.bind,
    ))
}

/// The local listener's request filter: [`authenticate`]'s Host and
/// cross-site checks, without the token or the reachability evidence (every
/// caller here is loopback).
async fn guard_local(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    check_host(&state.bootstrap, &request)?;
    check_cross_site(&state.bootstrap, &request)?;
    Ok(next.run(request).await)
}

/// Liveness. Unversioned and never authenticated.
async fn health(State(state): State<ApiState>) -> Response {
    let sway_up = state.sway.is_connected();
    let body = serde_json::json!({
        "status": if sway_up { "ok" } else { "degraded" },
        "sway": sway_up,
        "version": crate::VERSION,
    });
    // Still 200 when sway is down: the daemon is alive and will reconnect.
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// The app heartbeat route, which page content calls.
fn is_heartbeat(path: &str) -> bool {
    path.starts_with("/api/v1/apps/") && path.ends_with("/heartbeat")
}

/// The bundled UI, the Scalar docs page and the OpenAPI document: static
/// bytes compiled into the binary, holding no state and no credential (Scalar
/// embeds the OpenAPI document in its own page, so gating the JSON alone would
/// protect nothing). They are served without a token so that a browser
/// reaching the appliance directly gets the page that explains the 401 its API
/// calls then receive, instead of a bare JSON error. Every API call the UI
/// makes still needs the token, which only a proxy in front can supply.
fn is_static_page(path: &str) -> bool {
    matches!(
        path,
        "/" | "/favicon.ico" | "/docs" | "/api-docs/openapi.json"
    ) || path.starts_with("/assets/")
}

/// Paths reachable without a bearer token even when one is configured.
fn is_public(path: &str) -> bool {
    path == "/healthz"
        || is_static_page(path)
        // Heartbeats come from page content, which cannot hold the API token;
        // that endpoint is restricted to loopback callers instead.
        || is_heartbeat(path)
        // The capability check's page and report follow the same rule: page
        // content cannot hold the token, so both are loopback-restricted and
        // gated on the one-time id instead.
        || path.starts_with("/capability-check/")
        || (path.starts_with("/api/v1/capability-check/") && path.ends_with("/result"))
}

async fn authenticate(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // A request that arrived from off-box is proof the port is reachable —
    // something the reachability check cannot establish any other way, since
    // reading the host firewall's rules needs root. Recorded before the token
    // is examined: a rejected request crossed the network just as well.
    if let Some(peer) = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
    {
        state.checks.note_client(peer.0.ip());
    }

    // Before the token: these refuse what a browser was tricked into
    // sending, which matters most in open mode, where nothing else would.
    check_host(&state.bootstrap, &request)?;
    check_cross_site(&state.bootstrap, &request)?;

    let Some(expected) = state.bootstrap.token.as_deref() else {
        return Ok(next.run(request).await);
    };
    if is_public(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);

    match presented {
        Some(token) if constant_time_eq(token, expected) => Ok(next.run(request).await),
        _ => Err(ApiError::Unauthorized),
    }
}

/// The authority the request was addressed to: the `Host` header, or the
/// request target's own authority when a client sent an absolute URI.
fn request_authority(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        })
}

/// The host part of an authority, without the port; an IPv6 literal keeps
/// its brackets, as `allowed_hosts` stores it.
fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map_or(authority, |end| &authority[..=end]);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

/// Whether `host` names this machine's loopback interface.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Refuse a request addressed to a host name the operator did not list.
///
/// The DNS-rebinding defense: a hostile name pointed at the appliance's
/// address makes a browser treat the appliance as that site, same origin
/// and all, so neither CORS nor [`check_cross_site`] can tell. Only the
/// `Host` header still says which name the browser thought it was talking
/// to. Loopback names always pass: Suede hands its own launched apps and the
/// capability check `127.0.0.1` URLs, and no rebinding can produce one.
fn check_host(bootstrap: &BootstrapConfig, request: &Request) -> ApiResult<()> {
    if bootstrap.allowed_hosts.is_empty() {
        return Ok(());
    }
    let Some(authority) = request_authority(request) else {
        return Err(ApiError::Forbidden(
            "request refused: it has no Host header, and allowed_hosts is set; \
             send the host name the appliance is reached by"
                .into(),
        ));
    };
    let host = host_without_port(authority).to_ascii_lowercase();
    if is_loopback_host(&host) || bootstrap.allowed_hosts.contains(&host) {
        return Ok(());
    }
    Err(ApiError::Forbidden(format!(
        "request refused: Host {host:?} is not in allowed_hosts; add it to allowed_hosts \
         in suede.toml (or SUEDE_ALLOWED_HOSTS) and restart the daemon if this appliance \
         is meant to answer to that name"
    )))
}

/// Refuse a state-changing request a browser sent on behalf of a page from
/// another site.
///
/// CORS already stops a foreign page from sending JSON (that needs a
/// preflight this API never answers), but a form post, a `text/plain` body
/// or an empty POST is sent without asking, and the appliance would act on
/// it before the browser hid the response. So the browser's own account of
/// where the request came from is checked instead. `Sec-Fetch-Site` is the
/// authority when present, since every current browser sets it and a page
/// cannot forge it; it also keeps a reverse proxy that rewrites `Host` from
/// breaking the same-origin UI. Older browsers send only `Origin`, which is
/// compared with `Host`. A request with neither came from curl, a script or
/// another server, none of which a hostile page can drive, so it passes.
///
/// Heartbeats are exempt: page content served from anywhere, or from
/// `file://` (`Origin: null`), posts them by design, and the route is
/// loopback-only and changes nothing but a liveness timestamp.
fn check_cross_site(bootstrap: &BootstrapConfig, request: &Request) -> ApiResult<()> {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) || is_heartbeat(request.uri().path())
    {
        return Ok(());
    }
    let headers = request.headers();
    let origin = headers
        .get(header::ORIGIN)
        .map(|value| value.to_str().unwrap_or("").to_ascii_lowercase());
    let allowed = || {
        origin
            .as_deref()
            .is_some_and(|origin| bootstrap.allowed_origins.iter().any(|a| a == origin))
    };
    let origin_text = origin.as_deref().map_or_else(
        || "no Origin header".to_string(),
        |origin| format!("Origin {origin:?}"),
    );
    let remedy = "add the page's origin to allowed_origins in suede.toml (or \
                  SUEDE_ALLOWED_ORIGINS) and restart the daemon if that site is trusted";

    let fetch_site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());
    match fetch_site {
        Some(site @ ("cross-site" | "same-site")) => {
            if allowed() {
                return Ok(());
            }
            return Err(ApiError::Forbidden(format!(
                "cross-site request refused: the browser sent Sec-Fetch-Site {site:?} \
                 ({origin_text}); {remedy}"
            )));
        }
        // The browser vouches for the page being this origin, or for the
        // user having started the request themselves.
        Some("same-origin" | "none") => return Ok(()),
        _ => {}
    }

    let Some(origin) = origin.as_deref() else {
        return Ok(());
    };
    let own = request_authority(request).map(str::to_ascii_lowercase);
    if own
        .as_deref()
        .is_some_and(|own| same_authority(origin, own))
        || allowed()
    {
        return Ok(());
    }
    Err(ApiError::Forbidden(format!(
        "cross-origin request refused: Origin {origin:?} does not match Host {:?}; {remedy}",
        own.as_deref().unwrap_or("")
    )))
}

/// Whether a lowercase `Origin` names the lowercase `Host` authority, port
/// included. A port left implicit on one side and spelled out as the
/// scheme's default on the other still matches.
fn same_authority(origin: &str, host: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        // `null`, from a sandboxed or `file://` page, names no host at all.
        return false;
    };
    let default_port = match scheme {
        "http" => ":80",
        "https" => ":443",
        _ => return false,
    };
    authority.strip_suffix(default_port).unwrap_or(authority)
        == host.strip_suffix(default_port).unwrap_or(host)
}

/// Compare without leaking length or position through timing.
fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::audio::mock::MockAudio;
    use crate::checks::CheckRunnerDeps;
    use crate::reconciler::{ReconcileRequests, ReconcilerDeps};
    use crate::supervisor::LaunchContext;
    use crate::sway::mock::MockSway;

    pub struct Harness {
        pub state: ApiState,
        pub router: Router,
        pub sway: Arc<MockSway>,
        pub audio: Arc<MockAudio>,
        pub power: Arc<observed::power_mock::MockPower>,
        /// The receiving half of `state.trigger`'s channel, kept (rather than
        /// dropped, as it used to be) so a test can inspect what a write
        /// actually signaled: an ordinary debounced request via `try_recv`,
        /// or the projection `Notify` via `projection_handle`.
        pub requests: ReconcileRequests,
        pub _dir: tempfile::TempDir,
    }

    /// A fully wired API backed by mocks, with no background tasks running.
    pub fn harness(token: Option<&str>) -> Harness {
        harness_with_power(token, &[])
    }

    /// Like [`harness`], but with `bootstrap.power` set to `verbs` — the
    /// `POST /system/power` tests need every permission combination from
    /// empty (the default) to fully permitted.
    pub fn harness_with_power(token: Option<&str>, verbs: &[crate::model::PowerVerb]) -> Harness {
        harness_with_bootstrap(|bootstrap| {
            bootstrap.token = token.map(str::to_string);
            bootstrap.power = verbs.to_vec();
        })
    }

    /// Like [`harness`], with whatever bootstrap settings `configure` makes
    /// — for request filtering that only a bootstrap key turns on.
    pub fn harness_with_bootstrap(configure: impl FnOnce(&mut BootstrapConfig)) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let mut bootstrap = BootstrapConfig {
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        };
        configure(&mut bootstrap);
        let bootstrap = Arc::new(bootstrap);
        let sway = Arc::new(MockSway::with_fixtures());
        let audio = Arc::new(MockAudio::with_devices());
        let store = Arc::new(StateStore::ephemeral(dir.path().to_path_buf()));
        let snapshot = Arc::new(Snapshot::new());
        let hub = EventHub::new();
        let wallpapers = Arc::new(crate::wallpapers::WallpaperStore::new(
            dir.path().join("wallpapers"),
        ));
        let supervisor = Arc::new(Supervisor::new(
            sway.clone(),
            hub.clone(),
            LaunchContext {
                profiles_root: dir.path().join("profiles"),
                log_root: dir.path().join("logs"),
                // What `main` hands launched apps: the local listener.
                api_base: format!(
                    "http://{}/api/v1",
                    bootstrap.local_bind().expect("a valid local listener")
                ),
                browser_devtools: true,
            },
            // Unrestricted: API-level tests exercise the app lifecycle
            // endpoints, not the allowlist, and launch stand-in commands a
            // real appliance's browser-only default would refuse.
            vec!["*".to_string()],
        ));
        let reconciler = Arc::new(Reconciler::new(ReconcilerDeps {
            sway: sway.clone(),
            audio: audio.clone(),
            store: store.clone(),
            snapshot: snapshot.clone(),
            supervisor: supervisor.clone(),
            events: hub.clone(),
            wallpapers: wallpapers.clone(),
            docs_base_url: bootstrap.docs_base_url.clone(),
            allow_overlaps: bootstrap.allow_overlaps,
        }));
        let capability_store = Arc::new(crate::capabilities::CapabilityStore::new(dir.path()));
        let (trigger, requests) = Reconciler::channel();
        let checks = Arc::new(CheckRunner::new(CheckRunnerDeps {
            bootstrap: bootstrap.clone(),
            sway: sway.clone(),
            audio: audio.clone(),
            store: store.clone(),
            events: hub.clone(),
            capabilities: capability_store.clone(),
            snapshot: snapshot.clone(),
            trigger: trigger.clone(),
            supervisor: supervisor.clone(),
        }));

        let power = Arc::new(observed::power_mock::MockPower::default());

        let state = ApiState {
            profiles: Arc::new(crate::profiles::ProfileStore::new(
                dir.path().join("system-profiles"),
            )),
            bootstrap,
            store,
            snapshot,
            events: hub,
            sway: sway.clone(),
            audio: audio.clone(),
            supervisor,
            reconciler,
            trigger,
            checks,
            wallpapers,
            capabilities: Arc::new(capabilities::CapabilityChecks::default()),
            capability_store,
            power: power.clone(),
            started_at: Instant::now(),
        };

        Harness {
            router: router(state.clone()),
            state,
            sway,
            audio,
            power,
            requests,
            _dir: dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::harness;
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn raster_lookup_falls_back_to_the_observed_current_mode() {
        let snapshot = Snapshot::new();
        snapshot.set_outputs(vec![crate::model::Output {
            name: "HDMI-A-1".into(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: Some(crate::model::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0,
            }),
            modes: vec![],
            rect: crate::model::Rect::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }]);
        let lookup = raster_lookup(&snapshot);

        // No configured mode: falls back to the observed current mode,
        // matched by the output's own match rule.
        let configured = OutputConfig::new(crate::model::OutputMatch::by_name("HDMI-A-1"));
        assert_eq!(lookup(&configured), Some((1920, 1080)));

        // A configured mode wins over the observed one.
        let mut with_mode = configured.clone();
        with_mode.mode = Some(crate::model::Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 60.0,
        });
        assert_eq!(lookup(&with_mode), Some((3840, 2160)));

        // No match at all: nothing to report.
        let unmatched = OutputConfig::new(crate::model::OutputMatch::by_name("HDMI-A-9"));
        assert_eq!(lookup(&unmatched), None);
    }

    #[test]
    fn only_projection_previews_bypass_topology_debounce() {
        let previous = DesiredState::default();
        let mut next = previous.clone();
        next.projection = Some(crate::model::ProjectionConfig::default());
        next.committed = !previous.committed;
        assert!(projection_only_change(&previous, &next));
        next.settings.hide_cursor = !previous.settings.hide_cursor;
        assert!(!projection_only_change(&previous, &next));
        next = previous.clone();
        next.outputs.push(crate::model::OutputConfig::new(
            crate::model::OutputMatch::default(),
        ));
        assert!(!projection_only_change(&previous, &next));

        let previous: DesiredState =
            serde_json::from_str(include_str!("../../docs/examples/four-output-warp.json"))
                .unwrap();
        let mut next = previous.clone();
        next.outputs[0].geometry.as_mut().unwrap().corners[0][0] += 0.01;
        assert!(projection_only_change(&previous, &next));
        next.outputs[0].position.as_mut().unwrap().x += 1;
        assert!(!projection_only_change(&previous, &next));
    }

    #[tokio::test]
    async fn healthz_is_open() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["version"], crate::VERSION);
    }

    #[tokio::test]
    async fn healthz_stays_open_in_token_mode() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_token_configured_means_no_authentication() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_mode_rejects_an_anonymous_request() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_mode_accepts_the_right_token() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_mode_rejects_a_wrong_token() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .header("authorization", "Bearer wrong!")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn errors_are_problem_json() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs/NOPE")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/problem+json"
        );
        let body = body_json(response).await;
        assert_eq!(body["status"], 404);
        assert!(body["type"].as_str().unwrap().contains("not-found"));
    }

    #[tokio::test]
    async fn scalar_docs_are_served_without_a_token() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/docs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn docs_and_the_openapi_document_are_served_in_token_mode_without_a_token() {
        // Static documents with no credential in them (Scalar embeds the
        // OpenAPI document itself), so they follow the UI.
        for path in ["/docs", "/api-docs/openapi.json"] {
            let harness = harness(Some("secret"));
            let response = harness
                .router
                .oneshot(
                    HttpRequest::builder()
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }

    #[test]
    fn heartbeat_paths_are_public() {
        assert!(is_public("/api/v1/apps/renderer-1/heartbeat"));
        assert!(is_public("/healthz"));
        assert!(!is_public("/api/v1/apps/renderer-1/restart"));
        assert!(!is_public("/api/v1/config"));
    }

    #[test]
    fn capability_check_paths_are_public_but_starting_one_is_not() {
        // The page and its report come from the appliance's own browser,
        // which cannot hold the token; both are loopback-gated instead.
        assert!(is_public("/capability-check/abc123"));
        assert!(is_public("/api/v1/capability-check/abc123/result"));
        // Launching a browser on the displays is an operator action.
        assert!(!is_public("/api/v1/apps/capabilities"));
    }

    /// A POST to `/api/v1/reconcile` from a browser, carrying whatever
    /// headers the case under test needs. `reconcile` has no body and needs
    /// no token in open mode, which is what made it the original exploit.
    fn reconcile_post(headers: &[(&str, &str)]) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/api/v1/reconcile")
            .header("host", "appliance.local:9088");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn status_of(
        harness: super::test_support::Harness,
        request: HttpRequest<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let response = harness.router.oneshot(request).await.unwrap();
        let status = response.status();
        (status, body_json(response).await)
    }

    #[tokio::test]
    async fn a_cross_origin_form_post_is_refused_naming_the_header_and_the_setting() {
        let (status, body) = status_of(
            harness(None),
            reconcile_post(&[
                ("origin", "http://evil.example"),
                ("content-type", "application/x-www-form-urlencoded"),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["status"], 403);
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains("http://evil.example"), "{detail}");
        assert!(detail.contains("Host"), "{detail}");
        assert!(detail.contains("allowed_origins"), "{detail}");
    }

    #[tokio::test]
    async fn a_post_without_an_origin_header_succeeds_as_curl_sends_it() {
        let (status, _) = status_of(
            harness(None),
            reconcile_post(&[("content-type", "application/x-www-form-urlencoded")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_same_origin_post_from_an_older_browser_succeeds() {
        let (status, _) = status_of(
            harness(None),
            reconcile_post(&[("origin", "http://appliance.local:9088")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_same_origin_post_succeeds_even_when_a_proxy_rewrote_the_host() {
        // A proxy that forwards with its own upstream `Host` makes `Origin`
        // and `Host` disagree; the browser's `Sec-Fetch-Site` still vouches.
        let (status, _) = status_of(
            harness(None),
            reconcile_post(&[
                ("origin", "https://suede.example.com"),
                ("sec-fetch-site", "same-origin"),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_post_from_an_allowed_origin_succeeds() {
        let harness = super::test_support::harness_with_bootstrap(|bootstrap| {
            bootstrap.allowed_origins = vec!["https://control.example.com".into()];
        });
        let (status, _) = status_of(
            harness,
            reconcile_post(&[
                ("origin", "https://control.example.com"),
                ("sec-fetch-site", "cross-site"),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_allowed_origin_also_passes_an_older_browser_without_sec_fetch_site() {
        let harness = super::test_support::harness_with_bootstrap(|bootstrap| {
            bootstrap.allowed_origins = vec!["https://control.example.com".into()];
        });
        let (status, _) = status_of(
            harness,
            reconcile_post(&[("origin", "https://control.example.com")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn sec_fetch_site_cross_site_is_refused_even_without_an_origin() {
        let (status, body) = status_of(
            harness(None),
            reconcile_post(&[("sec-fetch-site", "cross-site")]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains("Sec-Fetch-Site"), "{detail}");
        assert!(detail.contains("allowed_origins"), "{detail}");
    }

    #[tokio::test]
    async fn sec_fetch_site_same_site_is_refused_unless_the_origin_is_allowed() {
        let (status, _) = status_of(
            harness(None),
            reconcile_post(&[
                ("origin", "http://other.local:9088"),
                ("sec-fetch-site", "same-site"),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_foreign_origin_cannot_post_in_token_mode_even_with_the_token() {
        // Refused before the token is looked at: a browser that holds a
        // token (through a proxy that injects it) is the one to protect.
        let (status, body) = status_of(
            harness(Some("secret")),
            reconcile_post(&[
                ("origin", "http://evil.example"),
                ("authorization", "Bearer secret"),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["detail"].as_str().unwrap().contains("allowed_origins"));
    }

    #[tokio::test]
    async fn a_get_with_a_foreign_origin_succeeds() {
        let request = HttpRequest::builder()
            .uri("/api/v1/status")
            .header("host", "appliance.local:9088")
            .header("origin", "http://evil.example")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .unwrap();
        let (status, _) = status_of(harness(None), request).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_origin_matches_a_host_that_spells_out_the_default_port() {
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/api/v1/reconcile")
            .header("host", "appliance.local:80")
            .header("origin", "http://appliance.local")
            .body(Body::empty())
            .unwrap();
        let (status, _) = status_of(harness(None), request).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_null_origin_is_refused_outside_the_heartbeat_route() {
        let (status, _) = status_of(harness(None), reconcile_post(&[("origin", "null")])).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn any_host_is_answered_while_allowed_hosts_is_empty() {
        let request = HttpRequest::builder()
            .uri("/api/v1/status")
            .header("host", "rebound.evil.example:9088")
            .body(Body::empty())
            .unwrap();
        let (status, _) = status_of(harness(None), request).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn allowed_hosts_refuses_an_unlisted_host_naming_the_setting() {
        let harness = super::test_support::harness_with_bootstrap(|bootstrap| {
            bootstrap.allowed_hosts = vec!["suede.example.com".into()];
        });
        // A GET too: rebinding is about reading the API as much as writing.
        let request = HttpRequest::builder()
            .uri("/api/v1/status")
            .header("host", "rebound.evil.example:9088")
            .body(Body::empty())
            .unwrap();
        let (status, body) = status_of(harness, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains("rebound.evil.example"), "{detail}");
        assert!(detail.contains("allowed_hosts"), "{detail}");
    }

    #[tokio::test]
    async fn allowed_hosts_accepts_a_listed_host_on_any_port_and_any_case() {
        let harness = super::test_support::harness_with_bootstrap(|bootstrap| {
            bootstrap.allowed_hosts = vec!["suede.example.com".into()];
        });
        let request = HttpRequest::builder()
            .uri("/api/v1/status")
            .header("host", "Suede.Example.com:9088")
            .body(Body::empty())
            .unwrap();
        let (status, _) = status_of(harness, request).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn allowed_hosts_always_accepts_loopback_names() {
        // Launched apps and the capability check are handed 127.0.0.1 URLs
        // by Suede itself.
        for host in ["127.0.0.1:9088", "localhost:9088", "[::1]:9088"] {
            let harness = super::test_support::harness_with_bootstrap(|bootstrap| {
                bootstrap.allowed_hosts = vec!["suede.example.com".into()];
            });
            let request = HttpRequest::builder()
                .uri("/api/v1/status")
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let (status, _) = status_of(harness, request).await;
            assert_eq!(status, StatusCode::OK, "{host}");
        }
    }

    /// `request` from a loopback peer, as either listener's server would
    /// deliver it.
    fn from_loopback(mut request: HttpRequest<Body>) -> HttpRequest<Body> {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            ))));
        request
    }

    async fn local_status_of(
        harness: &super::test_support::Harness,
        request: HttpRequest<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let response = local_router(harness.state.clone())
            .oneshot(from_loopback(request))
            .await
            .unwrap();
        let status = response.status();
        (status, body_json(response).await)
    }

    #[tokio::test]
    async fn the_local_listener_serves_nothing_but_the_app_facing_routes() {
        let harness = harness(None);
        for (method, uri) in [
            ("GET", "/healthz"),
            ("GET", "/"),
            ("GET", "/docs"),
            ("GET", "/api-docs/openapi.json"),
            ("GET", "/api/v1/status"),
            ("GET", "/api/v1/config"),
            ("GET", "/api/v1/events"),
            ("POST", "/api/v1/reconcile"),
            ("POST", "/api/v1/apps/renderer/restart"),
            ("POST", "/api/v1/apps/capabilities"),
        ] {
            let request = HttpRequest::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let (status, body) = local_status_of(&harness, request).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
            let detail = body["detail"].as_str().unwrap_or_default();
            assert!(
                detail.contains("only app heartbeats and the capability check"),
                "{method} {uri}: {detail}"
            );
            assert!(detail.contains("0.0.0.0:9088"), "{method} {uri}: {detail}");
        }
    }

    #[tokio::test]
    async fn the_local_listener_reaches_the_app_facing_handlers() {
        // Each answers from its own handler, not the fallback: an unknown
        // app, and checks nobody is waiting for.
        let harness = harness(None);
        for (method, uri, expected) in [
            ("POST", "/api/v1/apps/ghost/heartbeat", "no app named ghost"),
            (
                "POST",
                "/api/v1/capability-check/deadbeef/result",
                "no capability check",
            ),
            ("GET", "/capability-check/deadbeef", "no capability check"),
        ] {
            let mut builder = HttpRequest::builder().method(method).uri(uri);
            let body = if method == "POST" && uri.ends_with("/result") {
                builder = builder.header("content-type", "application/json");
                Body::from(
                    r#"{"userAgent":"t","webgpu":false,"videoDecoderApi":false,"notes":[],"codecs":[]}"#,
                )
            } else {
                Body::empty()
            };
            let (status, body) = local_status_of(&harness, builder.body(body).unwrap()).await;
            let detail = body["detail"].as_str().unwrap_or_default();
            assert!(
                status == StatusCode::NOT_FOUND && detail.contains(expected),
                "{method} {uri}: {status} {detail}"
            );
        }
    }

    #[tokio::test]
    async fn the_local_listener_asks_for_no_token() {
        let harness = harness(Some("secret"));
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/api/v1/apps/ghost/heartbeat")
            .body(Body::empty())
            .unwrap();
        let (status, body) = local_status_of(&harness, request).await;
        // The handler's own 404, not a 401.
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["detail"].as_str().unwrap().contains("no app named"));
    }

    #[tokio::test]
    async fn the_local_listener_still_refuses_a_cross_site_capability_result() {
        // A page elsewhere must not be able to use this machine's browser to
        // post a report; heartbeats are the only cross-site route.
        let harness = harness(None);
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/api/v1/capability-check/deadbeef/result")
            .header("host", "127.0.0.1:9089")
            .header("origin", "http://evil.example")
            .header("sec-fetch-site", "cross-site")
            .header("content-type", "text/plain")
            .body(Body::from("{}"))
            .unwrap();
        let (status, body) = local_status_of(&harness, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["detail"].as_str().unwrap().contains("allowed_origins"));
    }

    #[tokio::test]
    async fn a_loopback_main_bind_sends_the_capability_routes_to_the_local_listener() {
        for uri in [
            "/capability-check/deadbeef",
            "/api/v1/capability-check/deadbeef/result",
        ] {
            let harness = super::test_support::harness_with_bootstrap(|bootstrap| {
                bootstrap.bind = "127.0.0.1:9088".parse().unwrap();
            });
            let method = if uri.ends_with("/result") {
                "POST"
            } else {
                "GET"
            };
            let request = HttpRequest::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"userAgent":"t","webgpu":false,"videoDecoderApi":false,"notes":[],"codecs":[]}"#))
                .unwrap();
            let response = harness
                .router
                .clone()
                .oneshot(from_loopback(request))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            let detail = body_json(response).await["detail"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(detail.contains("local listener"), "{uri}: {detail}");
            assert!(detail.contains("127.0.0.1:9089"), "{uri}: {detail}");
        }
    }

    #[tokio::test]
    async fn a_routable_main_bind_keeps_serving_the_capability_routes() {
        let harness = harness(None);
        let request = HttpRequest::builder()
            .uri("/capability-check/deadbeef")
            .body(Body::empty())
            .unwrap();
        let response = harness
            .router
            .clone()
            .oneshot(from_loopback(request))
            .await
            .unwrap();
        // The handler's own answer: nothing is waiting under that id.
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_json(response).await["detail"]
            .as_str()
            .unwrap()
            .contains("no capability check"));
    }

    #[test]
    fn host_without_port_keeps_ipv6_brackets() {
        assert_eq!(host_without_port("example.com:9088"), "example.com");
        assert_eq!(host_without_port("example.com"), "example.com");
        assert_eq!(host_without_port("[fd00::1]:9088"), "[fd00::1]");
        assert_eq!(host_without_port("[fd00::1]"), "[fd00::1]");
    }

    #[test]
    fn constant_time_comparison_is_correct() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(constant_time_eq("", ""));
    }

    #[tokio::test]
    async fn stale_if_match_is_a_conflict() {
        let harness = harness(None);
        harness.state.store.update(|_| {}).unwrap();
        assert!(harness.state.check_precondition(Some("1")).is_ok());
        assert!(matches!(
            harness.state.check_precondition(Some("0")),
            Err(ApiError::Conflict(_))
        ));
        assert!(harness.state.check_precondition(None).is_ok());
    }

    #[tokio::test]
    async fn non_numeric_if_match_is_rejected() {
        let harness = harness(None);
        assert!(matches!(
            harness.state.check_precondition(Some("banana")),
            Err(ApiError::BadRequest(_))
        ));
    }
}
