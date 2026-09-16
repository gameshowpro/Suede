# Heartbeat-feature feedback: validated findings and remediation

Source: feedback from an external implementer of an app using Suede's heartbeat
watchdog. Every claim was validated against the code before this plan was
written. Verdicts:

| # | Claim | Verdict | Remedy |
|---|-------|---------|--------|
| 1 | Readiness not re-probed on relaunch | TRUE | code + docs (slice 1) |
| 2 | /apps/{id}/status 404s after activate; no ?wait= | TRUE in substance (404 only for a never-reconciled app; more often a *stale* status) | code + docs (slice 4) |
| 3 | No CORS on heartbeat | TRUE (no CORS anywhere; preflight → 405). "must use no-cors" is wrong: a bare POST is a simple request and is delivered today | code + docs (slice 2) |
| 4 | "Is my app on the screens" needs three calls | TRUE (Status has no app fields; divergences only cover failure states) | code + docs (slice 3) |
| 5 | Exact refresh rates are brittle | FALSE (resolve_mode already matches nearest within 1 Hz, tested with 59.939/60.0; docs examples use 60, the 59.95 is an adopted value) | docs only (slice 5) |
| 6 | Deactivation stamps lastRestartReason: configChanged | TRUE | code (folded into slice 1, same function) |

Conventions:
- Run `cargo test` (whole crate) before reporting done. Run `cargo clippy --all-targets` and fix new warnings.
- Any change to a request/response shape: `UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot`, then check the snapshot diff is only what you meant.
- Docs: `docs/configuration.md` is the operator reference, `docs/specification.md` is the spec. Match their voice: short, declarative, says *why*. No em-dashes in new prose.
- Comments explain why, not what. Keep the existing comment style.
- Do not commit.

## Slice 1 — Re-probe readiness on every launch; do not mislabel deactivation  (model: sonnet — precise spec, one file, but a state machine with tests to write)

Files: `src/supervisor/mod.rs`, `docs/configuration.md` (section "Waiting for a service to be ready", ~line 655).

Today `ManagedApp.dependency_ready` latches true after the first acceptable probe
(or after `giveUpAfterSeconds`) and is only reset when the readiness config
changes. Every relaunch path (crash via `reap`, heartbeat timeout and
window-never-appeared via `check_watchdogs`, both through `schedule_restart_or_halt`;
manual `restart()`; and a non-readiness config change in `reconcile()`) skips the
probe. With the dependency down, a browser app relaunches into an error page,
sits out `startupGraceSeconds` (60 s), is killed by the watchdog, and loops.

Change: the probe gates *every* launch. Whenever an app is stopped and will be
launched again, clear `dependency_ready`, `last_probe`, and `waiting_since`.
The clean place is a small helper (e.g. `fn unlatch_readiness(managed)`) called
from `schedule_restart_or_halt`, `restart()`, and the `config_changed` branch in
`reconcile()` (which then no longer needs the `readiness != readiness` special
case; keep behaviour identical for that case). Also call it from the
deactivation path so a later re-activation probes afresh (see below).
Do not call it from `shutdown()`.

Rewrite the field comment on `dependency_ready`: it is no longer latched. The
original worry (a blip stalling a crash relaunch) is bounded: `tick()` probes
first, every second, so a healthy dependency delays a relaunch by about one
tick; an unhealthy one is exactly what should hold the launch. Say that.

`give_up_after_seconds` is measured from `waiting_since`, so clearing it means
the give-up timer restarts per launch. That is the intended reading ("gates
every launch") and needs one sentence in the docs.

Item 6, same function: in `reconcile()`'s `config_changed` branch, the only
diff for an app that lost `activeApp` is the synthesized `enabled` flag
(`Reconciler::effective_apps`, `src/reconciler/mod.rs` ~line 806). Stamping
`RestartReason::ConfigChanged` there is wrong: nothing about the app's own
config changed and it is not being restarted. Only set `ConfigChanged` when
the new config is `enabled`. A pure deactivation stops the app and leaves
`last_restart_reason` untouched. (Do NOT add an enum variant; no schema change.)

Docs: in "Waiting for a service to be ready", state plainly that the probe runs
before every launch, including relaunches after a crash, a heartbeat timeout, a
manual restart, or activation, so a dependency that dies takes the app back to
`waitingForDependency` instead of a browser error page; and that
`giveUpAfterSeconds` counts from the start of each wait.

Tests (in the existing `mod tests` of `src/supervisor/mod.rs`, using the same
helpers the readiness tests at ~1160–1310 use):
1. An app with readiness that is running, whose process then exits (or is
   restarted via `restart()`), with the readiness URL now failing, ends in
   `WaitingForDependency`, not `Starting`/`Running`, and its `pid` is `None`.
2. Same, but the URL still answers: the app relaunches (state `Starting` or
   `Running` after the tick), proving a healthy dependency does not block.
3. Deactivating an app (reconcile with its `enabled` flipped to false) stops it
   and leaves `last_restart_reason` at its previous value; a genuine config
   change on an enabled running app still records `ConfigChanged`.

Acceptance: all three tests pass; existing readiness and restart tests
unchanged and passing; `cargo test` green; docs updated.

## Slice 2 — CORS on the heartbeat route  (model: sonnet — small, but middleware placement and tests)

Files: `Cargo.toml` (line ~28: add `"cors"` to tower-http features), `src/api/mod.rs`
(router, ~line 122–233; `is_public` ~248), `src/api/apps.rs` (heartbeat handler
~240 and tests ~418–498), `docs/configuration.md` (heartbeat section ~771–780).

Add a `tower_http::cors::CorsLayer` scoped to `POST /apps/{id}/heartbeat` only
(not the whole API): `allow_origin(Any)`, `allow_methods([POST])`,
`allow_private_network(true)`. Verify `tower-http` 0.6 has
`allow_private_network` (it does: `~/.cargo/registry/src/*/tower-http-0.6.*/src/cors/mod.rs`).
Attach it so a preflight `OPTIONS` to that path is answered by the layer with
204 and the `Access-Control-Allow-*` headers rather than falling through to
405, and so the real POST response (204, 403, 404) carries
`Access-Control-Allow-Origin: *`. Make sure the `authenticate` middleware does
not reject the preflight (check `is_public` matches the path regardless of
method; adjust if needed). The rest of the API must gain no CORS headers.

Tests (add to `src/api/apps.rs` tests, using the existing helpers there):
1. `OPTIONS /api/v1/apps/{id}/heartbeat` with `Origin`,
   `Access-Control-Request-Method: POST` and
   `Access-Control-Request-Private-Network: true` → 2xx with
   `access-control-allow-origin: *`, `access-control-allow-methods` containing
   POST, and `access-control-allow-private-network: true`.
2. `POST .../heartbeat` with an `Origin` header from loopback → 204 with
   `access-control-allow-origin: *`; a 404 for an unknown id also carries it.
3. `GET /api/v1/status` (or any other route) with an `Origin` header carries no
   `access-control-allow-origin`.

Docs: after the existing snippet in the heartbeat section, add that the
endpoint answers cross-origin requests from any origin, including Chromium's
private-network preflight, so page content served from another host or port
can post with plain `fetch` and read the status: 404 means the app id in the
URL is wrong, 403 means the request did not arrive from loopback. Suggest
logging a non-2xx so a misconfiguration is visible in the page console. Do not
recommend `mode: "no-cors"`.

Acceptance: tests pass; snapshot test still passes (no schema change expected;
if utoipa picks up anything, regenerate and confirm the diff is nil or
intended); `cargo test` green.

## Slice 3 — Status carries the active app and its state  (model: sonnet — touches model, reconciler loop, API, docs, snapshot)

Files: `src/model/observed.rs` (`Status` ~341–390, `AppState`, `AppStatus`),
`src/reconciler/mod.rs` (`reconcile()` ~215–240 where `Status` is built; the
task loop tick branch ~1099–1106; `publish_status` ~1046), `src/api/observed.rs`
(`get_status` ~75–92), `docs/specification.md` (the `/status` row ~90 and
"Is the appliance doing what I asked?" ~96–110; the `status_changed` line ~147),
`tests/openapi.snapshot.json` (regenerate).

Add to `Status`:
```rust
/// The app `activeApp` names, and how it is doing, so "is my app on the
/// screens" is one call: `state == running` here beside `synced` above.
/// `None` when no app is active. Full detail (pid, restarts, window ids)
/// stays on `GET /apps/{id}/status`.
pub active_app: Option<ActiveApp>,
```
with `pub struct ActiveApp { pub id: String, pub state: AppState, pub detail: Option<String> }`
(camelCase, `ToSchema`, same derives as neighbours). Serialize `activeApp` as
`null` when absent (not skipped), matching how `lastReconciled` is handled;
check the snapshot for how nullables are declared and follow suit.

Populate it:
- In `Reconciler::reconcile()` wherever the final `Status` is published: from
  `desired.active_app` and `self.supervisor.status(&id)`. An id with no
  supervisor entry yet gives `state: stopped`, `detail: None` (or whatever
  `AppStatus` reports for a never-launched app; pick the truthful one).
- In `get_status` (API): refresh `active_app` live, like `committed`, so a
  read between passes is current.
- Between passes: the supervisor `tick()` runs every second and can change the
  active app's state (heartbeat timeout, window appeared, backoff). Today the
  loop only re-reconciles when `fault_signature()` changes. Extend that branch
  so a change in the active app's `state` also republishes `Status` (the
  cheapest honest route: after `supervisor.tick`, read `snapshot.status()`,
  recompute `active_app`, and `publish_status` if it differs; `publish_status`
  already dedups). Do not trigger a full reconcile just for this.

Docs: in the `/status` row list `activeApp`. In "Is the appliance doing what I
asked?" add a fifth bullet: `activeApp.state == running` (the app is up; null
means nothing is active). Note that `status_changed` is republished when the
active app's state changes, not only after a pass.

Tests:
1. Reconciler test (follow the style of existing reconciler tests around
   `active_app`): after a pass with an active app whose launcher is a
   permitted stand-in, `snapshot.status().active_app` has that id and a
   non-stopped state; with `active_app: None` it is `None`.
2. API test for `GET /status` including `activeApp` in the JSON.
3. `src/events.rs` serialization test still passes with the new field.

Acceptance: snapshot regenerated and the diff is only `Status`/`ActiveApp`;
`cargo test` green; docs updated.

## Slice 4 — `?wait=` on activate and deactivate  (model: sonnet — mechanical, plus docs and snapshot)

Depends on slice 3 (snapshot). Files: `src/api/apps.rs` (`activate_app` ~185,
`deactivate_app` nearby, their `#[utoipa::path]` params), `src/api/config_routes.rs`
(`WaitQuery` ~21, already `pub` and `IntoParams`), `docs/specification.md`,
`docs/configuration.md` (~390 where activate is described, ~128 where `?wait=` is described),
`tests/openapi.snapshot.json`.

Add `Query<WaitQuery>` to `activate_app` and `deactivate_app` and pass
`query.wait` to `state.commit(...)` instead of `None`. Add `params(WaitQuery)`
to their utoipa attributes so the snapshot documents it. Leave `restart_app`
alone: it already awaits `advance()` synchronously and returns 404 for an
unregistered app, which is correct.

Docs: `docs/specification.md` never documents activate/deactivate. Add them
under "Desired state (read-write)" (they persist `activeApp`), one line each,
noting they accept `?wait=` like other writes. Where `?wait=` is described,
say why a client wants it here: without it, `GET /apps/{id}/status` right
after `activate` reports the state from before the switch (or 404 for an app
added moments ago and not yet reconciled); with `?wait=`, the response returns
after the pass that launched the app. Mirror a short version of this in
`docs/configuration.md` at the activate description.

Tests: in `src/api/apps.rs`, `POST /apps/{id}/activate?wait=5` then
`GET /apps/{id}/status` → 200 (use the same in-memory reconciler harness the
config_routes wait tests use; if such a harness only exists in
`config_routes.rs` tests, reuse it). And `activate` without wait still 200s.

Acceptance: snapshot diff is only the new `wait` parameter on the two routes;
`cargo test` green; docs updated.

## Slice 5 — Document refresh-rate matching  (model: haiku — docs only, facts supplied here)

File: `docs/configuration.md`, the outputs table row for `mode` (~line 138) and
the adopted-values example (~245).

Facts (from `src/model/observed.rs` `resolve_mode`, lines ~107–133, and its
tests ~864–918): a requested `refreshHz` is resolved against the modes the
display advertises at that width and height. An exact match (within 0.01 Hz)
wins; otherwise the nearest advertised rate within 1 Hz is used and applied
*as advertised*, so asking for `60` on a panel offering 59.951 selects 59.951
and reports no divergence. If a display offers both 59.939 and 60.000, `60`
selects 60.000. Only a resolution the display does not offer, or a rate more
than 1 Hz from anything offered, raises `mode_unsupported`. `null` for the
whole `mode` leaves the preferred mode, which is then adopted.

Write: a short paragraph directly under the outputs table (or a `!!! note`
if the docs use admonitions; check the file) titled "Refresh rates" saying the
above in the docs' voice, and add one clause to the `mode` table row. In the
adopted-values example, add a comment or sentence making clear the `59.95` is
what Suede observed and pinned, not a value an operator has to know.

Acceptance: `cargo test --test docs_links` passes; the prose states the exact
0.01 Hz / 1 Hz rules and the "applied as advertised" behaviour.

## Order

Slice 1 and slice 5 first (5 is docs-only and touches a different section of
configuration.md). Then 2, then 3, then 4 (needs 3's snapshot). One compiling
slice at a time.

## Outcome (2026-09-15)

All five slices delivered, uncommitted. No escalations: slices 1–4 on sonnet,
slice 5 on haiku, each accepted first time after review. Post-review fixes by
the orchestrator: moved the "Refresh rates" subsection below the `match`
examples; rustfmt on `src/supervisor/mod.rs`; three wording corrections in the
docs (wait returns the document, not `/status`; "this app's state from before
the switch"; the no-cors sentence). Final: `cargo test` 555 + 6 + 2 passed,
clippy clean, fmt clean.
