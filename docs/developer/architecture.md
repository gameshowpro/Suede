# Architecture

## The central idea

Clients write *desired state*. A reconciler drives the live session toward it. Observed state is always re-derived and never persisted.

Everything else follows from that. Boot-restore, hotplug recovery, crash recovery, and API writes are not four features — they are four triggers for one pass.

## Module map

```
src/
├── main.rs           CLI (`run`, `openapi`), daemon wiring, signal handling
├── config.rs         Bootstrap configuration: file + SUEDE_* overrides
├── model/
│   ├── observed.rs   What sway and PipeWire report
│   └── desired.rs    The document clients write, and its validation
├── sway/
│   ├── protocol.rs   IPC framing
│   ├── raw.rs        Sway's JSON shapes, and the mapping into the model
│   ├── client.rs     Live client over a Unix socket
│   └── mock.rs       In-memory client that simulates output commands
├── audio/
│   ├── pw.rs         PipeWire via pw-dump / pw-cli
│   └── mock.rs       In-memory monitor
├── state.rs          Atomic persistence with a .bak fallback
├── snapshot.rs       Shared live view of outputs, windows, and status
├── reconciler/
│   ├── plan.rs       Pure diff: (observed, desired) → commands
│   └── mod.rs        The pass, the task loop, event forwarding
├── supervisor/
│   ├── launcher.rs   Launch specification → process invocation
│   └── mod.rs        Process lifecycle, placement, watchdog
├── checks/           Environment health checks and remediations
├── events.rs         SSE fan-out
└── api/              axum routers, handlers, OpenAPI, embedded web UI
```

## Boundaries that matter

**Everything that touches the compositor goes through `SwayClient`.** The trait has five methods; the live implementation speaks the IPC protocol over a Unix socket, and the mock records commands and simulates their effect. Nothing else in the codebase opens a socket. This is what lets CI — which has no compositor, no displays, and no audio server — run the full test suite.

**The planner is pure.** `plan_outputs(observed, desired, applied, capabilities)` returns a list of command strings and divergences. It performs no IO, so the reconciliation rules are table-testable:

```rust
let plan = plan_outputs(&observed, &desired, &applied, capabilities);
assert_eq!(plan.commands, vec![
    "output HDMI-A-1 enable",
    "output HDMI-A-1 mode 1920x1080@60Hz",
    "output HDMI-A-1 pos 0 0",
    ...
]);
```

The executor in `reconciler/mod.rs` runs the plan. Splitting them is what makes "does a satisfied configuration issue any commands?" a one-line assertion.

**Applications are child processes, not `exec` calls.** Sway's `exec` would leave Suede with no PID — the prior implementation this generalizes had to terminate browsers with `pkill -f chrom|firefox`. Owning the process gives clean SIGTERM-then-SIGKILL termination of the whole process group, exit-code observation, restart policies, and per-app audio routing through the environment.

## The reconciliation pass

1. Re-query outputs.
2. Diff against desired state; issue only the commands for fields that differ.
3. If any output was enabled or disabled, wait for the layout to settle, then re-query.
4. Resolve each app to its target output and workspace.
5. Start, stop, or restart applications accordingly.
6. Ensure the null audio sink exists, if any app routes to silence.
7. Hide and park the cursor.
8. Re-query windows, place any that are newly mapped, run the watchdog.
9. Publish status.

A failed command is logged, recorded as a divergence, and retried next pass. A pass never aborts the daemon and never discards desired state.

### Diffing against two sources

Most settings are diffed against **observed** state, which is authoritative and survives a daemon restart. But Sway does not report `tearing` or `max_render_time` back through `get_outputs`, so those are diffed against what Suede last applied, held in memory.

There is one subtlety worth knowing: enabling an output resets everything Sway knows about it. A plan that issues `enable` therefore re-applies every other setting unconditionally, rather than trusting a record that says they already match.

## Concurrency

| Task | Responsibility |
|---|---|
| HTTP server | Serves requests; writes go to the store and trigger a pass |
| Reconciler | Owns "make live match desired"; one pass at a time, under a mutex |
| Sway event pump | Reconnects with backoff, forwards events to the trigger |
| PipeWire monitor | `pw-dump --monitor` as a change trigger, debounced |
| Health checks | Re-evaluated every 60 seconds |

Triggers are coalesced: a burst of events produces one pass. The trigger channel has capacity one, and a full channel means a pass is already pending, so requesting is never blocking.

## Testing

CI has no compositor, so the architecture is built around that constraint rather than fighting it.

| Layer | How it is tested |
|---|---|
| Sway mapping | Recorded `get_outputs` / `get_tree` fixtures in `src/sway/fixtures/` |
| Planner | Table-driven; asserts exact command sequences |
| Supervisor | Real child processes (`sleep`, `true`), mock compositor |
| API | `tower::ServiceExt::oneshot` against the router, with mock backends |
| Persistence | Temp directories; corruption and migration paths included |
| OpenAPI | Snapshot test, so every endpoint change is a reviewable diff |
| End to end | `scripts/smoke-test.sh` drives a running daemon over HTTP |

The smoke test is the one that catches what unit tests structurally cannot: it starts the real binary, kills a supervised process to prove it relaunches, restarts the daemon to prove configuration survives, and diffs `suede openapi` against the served document.

```bash
scripts/dev-check.sh          # fmt, clippy, test, smoke
scripts/dev-check.sh snapshot # refresh the OpenAPI snapshot
```

## Deliberate deviations from the specification

Two dependencies named in the original specification were replaced during implementation. Both preserve a higher-level guarantee the specification also makes — a single self-contained binary with no native library dependencies, cross-compilable to aarch64 without a custom sysroot.

**`swayipc-async` → a direct implementation.** The crate is built on the smol/`futures-lite` ecosystem rather than tokio, so using it inside a tokio/axum service would mean two async reactors in one binary. The IPC protocol is a six-byte magic string, a little-endian length, and a little-endian type; the implementation in `sway/protocol.rs` is about 60 lines and gives exact control over reconnection.

**The `pipewire` crate → `pw-dump` and `pw-cli`.** The crate links against `libpipewire-0.3`, which would add a build-time native dependency, break the "only libc" property, and require a PipeWire-equipped arm64 sysroot for cross-compilation. Driving PipeWire's own command-line tools gives the same capabilities — enumeration, change notification, null sink creation — with no build dependency at all. `pw-dump --monitor` is used purely as a change *trigger*, mirroring how Sway's detail-free `output` event is handled, with a one-shot `pw-dump` providing the authoritative list.

**The web UI is build-step free.** The specification called for TypeScript compiled to static assets. It is instead one self-contained HTML file embedded with `include_str!`. A reference client's job is to be readable and to exercise every endpoint; requiring an npm toolchain in CI to ship it would be a poor trade.
