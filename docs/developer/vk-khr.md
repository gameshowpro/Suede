# VK_KHR direct display

Facts for developers deciding whether and how to use the direct Vulkan
presenter (`VK_KHR_display`) instead of the Sway/Wayland path. Distilled from
the research plans linked below; no claim here goes beyond what those plans
record. **Sway/Wayland remains the only path proven in production.**

## What's proven on each card

| | [System A](test-systems.md#system-a) (Quadro RTX 8000, Turing) | [System B](test-systems.md#system-b) (RTX A1000, Ampere) |
|---|---|---|
| OS / kernel | Ubuntu 26.04, kernel 7.0.0-34 | Debian 13, kernel 6.12 |
| NVIDIA driver | 595.91.07 (server packages) | 615.71.09 open kernel module (also tested: 550.163.01, 595.91.07, 610.57.04) |
| Outputs | 4× 1920×1080 @ 59.94 Hz | 3× 1920×1200 @ 59.95 Hz |
| `VK_KHR_display` presentation | Works | Works |
| `VK_NV_present_barrier` | Advertised end to end; first present fails | Advertised end to end; first present fails |
| `VK_KHR_display_swapchain` | Absent | Absent |
| `VK_EXT_present_timing` + `VK_KHR_present_id2` | Full per-present records, all 4 outputs | Works on 595, 610 and 615; not available on 550 |
| NVKMS sub-ownership tested | Yes | Yes (595, 610, 615) |
| GSP firmware off tested | Yes | Yes (595, 610; 615's open module has no GSP-off option) |
| Real slicer run over the direct path | Yes | Yes (595, 610, 615) |
| `presentation = "direct"` option validated (boot, fallback, recovery) | Yes | No |

[System B](test-systems.md#system-b) was first probed with ordinary presentation and barrier tests on 550,
then validated for direct on 595, 610 and 615 in a driver, GSP, `gl_yield` and
alignment campaign ([results](../plans/display-component-results.md#system-b-campaign-drivers-gsp-yield-and-alignment-september-30-2026)).
Direct needs GSP off to keep up at light load, so on 615 (open module only,
GSP always on) it is not competitive.

Shared, on both cards:

- `VK_KHR_display` presents to explicitly chosen connectors after taking DRM
  master and calling `vkAcquireDrmDisplayEXT`. Connector IDs are mapped with
  `vkGetDrmDisplayEXT`, not display names.
- `VK_NV_present_barrier`'s extension, device feature and surface support are
  all advertised and swapchain creation succeeds, but the first present fails
  with `VK_ERROR_UNKNOWN` (driver 550 on [System B](test-systems.md#system-b); 580 and 595 on [System A](test-systems.md#system-a)). See
  [present barrier root cause](../plans/display-baseline-results.md#present-barrier-root-cause).
- `VK_KHR_display_swapchain` is absent on both; the presenter uses one
  swapchain per connector, sharing the source canvas.

[System A](test-systems.md#system-a)-only (driver 595.91.07):

- `VK_EXT_present_timing` (rev 3) + `VK_KHR_present_id2` give complete
  per-present records on all four outputs at `IMAGE_FIRST_PIXEL_OUT`, in the
  `DEVICE` clock domain, mapped to `CLOCK_MONOTONIC` via periodic
  `VK_KHR_calibrated_timestamps` samples (~every 250 ms). Driver 580 exposed
  neither `EXT` nor `GOOGLE` timing; driver 550 on [System B](test-systems.md#system-b) lacks both
  `VK_EXT_present_timing` and `VK_KHR_present_id2`.
- The real slicer (Chrome capture, warp, blend) presents through the direct
  path, selected by `presentation = "direct"` (see [The option](#the-option)).
- NVKMS sub-ownership (see below) removes `nvidia-drm` `nv_flip == NULL`
  warnings and per-head 3 s flip timeouts: probe teardown dropped from 13.2 s
  to 0.9 s; a real-slicer 30 s run exited 0.7 s after run time with a clean
  kernel log.
- GSP firmware off drops per-present RM allocate/free from ~0.72/0.74 ms to
  ~0.05/0.12 ms, and probe steady-state present kernel CPU from ~17% to
  ~4.6% of a core across four heads. [System A](test-systems.md#system-a) now boots this way
  (`/etc/modprobe.d/nvidia-gsp-off.conf`). It has no measurable effect on
  the Wayland path, whose cost is Sway's EGL wait, not per-present kernel
  objects. Figures: [root cause and GSP measurements](../plans/display-baseline-results.md#present-barrier-root-cause).

## What it takes to get there

- **Driver:** the proprietary NVIDIA driver only. The barrier root cause below
  is source-verified through driver 615.71.09; no released driver changes it.
  Suede's own `nvidia-driver-version` health check compares the running
  driver against `NEWEST_TESTED_NVIDIA_DRIVER` (currently 615.71.09, the
  newest release these paths have been validated on) and warns when it is
  older.
- **Kernel config:** GSP firmware off (`NVreg_EnableGpuFirmware=0`), supported
  with the proprietary module only (verified on Turing and Ampere; NVIDIA's
  Debian packages for 615 and later ship only the open module).
- **Code mechanisms:**
  - Stop Sway on the physical outputs; a headless-only Sway keeps the Chrome
    capture canvas.
  - Open the DRM card as master, map connectors with `vkGetDrmDisplayEXT`,
    call `vkAcquireDrmDisplayEXT` for each, one FIFO swapchain per connector,
    and batch all presents in one `vkQueuePresentKHR`.
  - After acquiring all displays: `DRM_IOCTL_NVIDIA_GRANT_PERMISSIONS(SUB_OWNER)`
    with a fresh `/dev/nvidia-modeset` token, then
    `NVKMS_IOCTL_ACQUIRE_PERMISSIONS` on the driver's own NVKMS fd (found via
    `/proc/self/fd`). Granting before acquisition makes acquisition fail. The
    command index is 41 on 580 through 610 and 40 on 550 and 615 (615 dropped
    `CHECK_LUT_NOTIFIER`); Suede tries both, ordered by the running driver
    version. A wrong index is rejected on NVKMS's `paramSize` check before any
    handler runs. If the DRM grant is refused, direct presentation continues
    with per-head modeset permission. If the grant succeeds but no handle
    accepts the token, the cleanup revoke strips the driver's per-head modeset
    permission (every present would fail with `VK_ERROR_UNKNOWN`), so direct
    display creation fails and the daemon falls back with that reason.
  - Revoke sub-ownership before releasing displays — nvidia-drm rejects all
    atomic commits while granted, including Sway's. The presenter resets any
    stale grant at startup, and only issues the grant when
    `DRM_IOCTL_VERSION` reports `nvidia-drm`. Code:
    `src/projection/gpu/display/nvkms.rs`.
  - Timing: calibrate `DEVICE` timestamps to monotonic periodically; a single
    start/end calibration drifted 41.9 µs over 10 s and was rejected.

### Why the barrier fails

NVIDIA implements `VK_NV_present_barrier` as an NVKMS swap group
(source-verified; see
[present barrier root cause](../plans/display-baseline-results.md#present-barrier-root-cause)):

- NVKMS allows swap groups, flip-lock groups and framelock attributes only for
  the modeset owner or sub-owner. A DRM-acquired display gives only per-head
  modeset permission, so `ALLOC_SWAP_GROUP` returns `EPERM`.
- With sub-ownership forced, the swap group allocates, but the driver then
  sets the framelock attribute `FRAMELOCK_SYNC`, which NVKMS rejects with no
  framelock (Quadro Sync) device present. Faking that success leads to a
  user-space failure tied to `QuadroSync*` registry keys.
- NVKMS's own flip-lock group is a no-op on a single GPU
  ("TODO: enable fliplock for single GPUs"). No kernel path gives single-GPU
  cross-head flip lock outside swap groups.

## What a Quadro Sync board would add

- It is the missing piece for `VK_NV_present_barrier` on Linux: the driver's
  barrier path requires framelock hardware even on a single GPU.
- It would provide hardware flip lock (all heads flip together, or none) —
  true atomic wall updates, which neither path has today.
- It also provides framelock/genlock across GPUs and machines (house sync).
- [System A](test-systems.md#system-a)'s RTX 8000 accepts a Quadro Sync II board; [System B](test-systems.md#system-b)'s RTX A1000 has no
  sync connector, so that production analog cannot use one. Untested: no
  board has been fitted to either card.

## Conclusions: direct Vulkan path vs Sway/Wayland

- Sway/Wayland remains the default and the only path proven in production.
- Neither path has atomic wall updates: batched presents are not atomic, both
  rely on Suede's software gate, and hardware flip lock needs a Quadro Sync
  board.
- The direct path's advantages: authoritative per-image timestamps (driver
  595+) and no compositor in the output path. Its costs: driver-specific
  setup (sub-ownership, GSP off), exclusive output ownership, a dark gap on
  fallback, and the limits listed below.
- Best against best on [System A](test-systems.md#system-a), each path with its best Sway wait setting,
  two repeats
  ([attribution check](../plans/display-component-results.md#attribution-check-slice-p4-september-29-2026),
  [full matrix](../plans/display-component-results.md#best-versus-best-comparison-september-29-2026)):

  | Canvas | Wayland fps | Direct fps | Wayland CPU | Direct CPU | Wayland straddles | Direct straddles |
  | --- | ---: | ---: | ---: | ---: | ---: | ---: |
  | 4500×2679 | 49.2 / 49.7 | 52.3 / 52.1 | 46.3% / 46.3% | 50.8% / 50.7% | 99 / 78 | 85 / 67 |
  | 3200×1905 | 59.9 / 59.3 | 59.8 / 59.6 | 44.2% / 43.8% | 42.6% / 42.9% | 0 / 7 | 2 / 6 |

  CPU is total relevant process CPU as a share of one core. The 4500×2679
  gap is inside Wayland's own run-to-run spread (48.8–57.2 fps over four
  runs); CPU per frame is about equal; at 3200×1905 the two tie.
- The direct slicer spends about 10 points of one core more than the
  Wayland slicer, inside NVIDIA's present path (`vkQueuePresentKHR`, kernel
  Resource Manager). No application-side change reduced it; throttling the
  timing poll cost 4–7 fps and was reverted.
- `__GL_YIELD=USLEEP` in Sway's environment stops NVIDIA's EGL busy-wait and
  cuts Sway CPU on both paths ([System A](test-systems.md#system-a): total CPU 73.6% → 49.3% at 4500×2679,
  68.4% → 44.0% at 3200×1905; [System B](test-systems.md#system-b): Sway 71.8% → 9.7% under a saturated
  load, 30.1% → 18.8% under a light one), with no frame-rate loss. Straddles
  rose in three of the four measured cases, so it trades CPU for frame
  coherence. It favors neither path. Exposed as `gl_yield` in `suede.toml`
  (NVIDIA-only; see [NVIDIA `__GL_YIELD`](../configuration.md#gl-yield)).
- **Second-card campaign** ([System B](test-systems.md#system-b), 3 outputs,
  [full results](../plans/display-component-results.md#system-b-campaign-drivers-gsp-yield-and-alignment-september-30-2026)):
  - Best combination: Wayland with the outputs phase-aligned after session
    start, `gl_yield = usleep`, any driver 595 or newer (615 open
    recommended: newest and NVIDIA's supported module; GSP does not affect
    Wayland).
  - Phase alignment is the dominant factor: unaligned Wayland straddles
    31–56 per 10 s interval, aligned 0–6. Production Wayland is unaligned
    today; automatic alignment after every session start is the largest
    improvement found.
  - `usleep` costs about 1 straddle per interval under saturation and saves
    about 60 points of one core.
  - Direct verdict on the second card: it matches aligned Wayland only at
    light load with GSP off (proprietary module, driver 610 or older), never
    under saturation (best 14.4 fps against about 21), and is not competitive
    on 615's open module. Direct's heads hold a fixed 2–8 ms offset; aligned
    Wayland holds 0.04–0.15 ms.
- **Recommended setup today:** the
  [recommended NVIDIA profile](../configuration.md#recommended-nvidia-profile)
  (Wayland, outputs aligned at session start, `gl_yield = "usleep"`, driver
  615.71.09 or newer) — these are Suede's defaults.
- **Verdict: not the better path today; retest as drivers change.** On both
  tested NVIDIA cards — a large Turing card and a small Ampere card, on
  drivers 595 through 615 — direct presentation at best ties Wayland at light
  load and falls behind it under GPU saturation; it never wins on straddles,
  frame rate, or CPU per frame. On the larger card it ties throughout; on the
  smaller card its fixed present cost shows up as lost frames under load. Keep
  it as an experimental option and retest it when:
  - a new NVIDIA driver release ships (rerun the matched comparison before
    bumping `NEWEST_TESTED_NVIDIA_DRIVER`), especially one that lowers the
    present path's cost or lets the open kernel module run without GSP;
  - a Quadro Sync board is fitted (hardware flip lock is the one advantage
    Wayland cannot match);
  - a non-NVIDIA driver with `VK_EXT_present_timing` becomes available.

## The option

`presentation = "direct"` in `suede.toml` (with `allow_overlaps = true`) is
chosen at login: the tty1 profile starts a headless-only Sway for the canvas,
and the daemon resolves the same keys against the compositor it finds — see
[Experimental: direct presentation](../configuration.md#experimental-direct-presentation)
for the config key itself, and [the profile
block](deployment.md#the-login-profile-block) for how the login session
derives its environment from it. There is no `SUEDE_*` override and no API
switch: it describes how the compositor was started, which nothing on
Suede's own process can change after the fact. `GET /api/v1/system` reports
`presentation.requested`, `.effective`, `.reason` and `.outputs`.

The daemon reads the live display inventory itself — EDID identity and exact
mode timings straight from sysfs and `DRM_IOCTL_MODE_GETCONNECTOR` — rather
than trusting a prior Wayland boot's record, because in direct mode there is
no prior Wayland boot on the physical outputs to record. See
`src/drm_inventory.rs` and
`src/sway/direct.rs` (the `DirectOutputs` wrapper
that intercepts commands for owned outputs and simulates their effect, so the
reconciler, planner and `/outputs` need no direct-mode branch of their own).

## Limits (experimental)

Out of scope for this option (the last two bullets are observed behavior,
not scope):

- **Selection.** No `auto` mode; no switching between `wayland` and `direct`
  via the API or the web UI; no change to Wayland's own defaults.
- **While running direct:** hotplug (an unplug may trigger fallback);
  mode, scale, transform or adaptive-sync changes on an owned output; tearing;
  backgrounds (`bg` is a no-op on an owned output); re-verifying EDID after
  startup; more than one GPU or DRM card.
- **Synchronization.** No present barrier, flip lock, Quadro Sync support,
  synchronized cold start, or phase guarantee — see
  [Conclusions](#conclusions-direct-vulkan-path-vs-swaywayland) above for why
  the barrier does not work on this hardware at all.
- **Fallback.** No dark-gap-free fallback (the wall is dark roughly 10–20 s);
  no stall detection (a session that is alive but has stopped presenting is
  not distinguished from one that is healthy but idle); the console may show
  while the slicer restarts, and each restart costs about 5 s of driver
  enumeration.
- **Privileges and packaging.** No new privileges (no sudoers rule,
  `CAP_SYS_ADMIN`, or libseat); no systemd unit for Sway itself;
  `postinst` never re-provisions an appliance already on disk; there is no
  `provision.sh` flag for it — the key is written into `suede.toml` by hand.
- **Drivers.** Non-NVIDIA drivers are allowed but untested. NVIDIA drivers
  older than 595 lack `VK_EXT_present_timing` and `VK_KHR_present_id2`; on
  [System B](test-systems.md#system-b)'s driver 550 the slicer refuses with
  that reason and the session falls back to Wayland (verified, about 3.7 s
  from session switch to the ordinary compositor). On machines with more
  than one GPU the headless canvas must be allocated on the card that drives
  the wall — see [the login profile block](deployment.md#the-login-profile-block).
- **Static content never confirms a session.** Confirmation needs a stats
  interval with `presentationBackend: "vulkan-display"` and frames presented;
  an idle black wall, a static pattern, or an app that draws nothing reports
  no intervals, so it stays unconfirmed (and presents correctly). Any slicer
  exit in that state falls back at once.
- **Window placement chatter.** arena-fx's "Can't update Chrome" update
  bubble and its main window share the single headless workspace
  (`HEADLESS-1`); only one can be fullscreen at a time, so the supervisor
  logs "placed window" roughly once a second and the applied-window
  divergence never clears. Harmless — the same signature settles normally
  under the ordinary DRM session — but expected only in direct mode, where
  there is exactly one compositor workspace for every window an app opens.

## Session lifecycle of the experimental option

- **Idle wall.** With no active app and no test pattern, the daemon keeps a
  slicer running that presents black, so the displays stay owned and the
  console never shows through. Only a layout with no enabled output leaves
  the displays unowned.
- **Fallback.** Any slicer exit before confirmation, 3 unexpected exits
  within 10 minutes after it, a refused display preflight, or a slicer
  configuration that cannot be derived ends the direct session: the daemon
  writes `$XDG_RUNTIME_DIR/suede/presentation-fallback`, stops the slicer,
  runs `suede display-reset`, and asks the headless Sway to exit. Auto-login
  then starts the ordinary DRM Sway, and `GET /system` reports effective
  `wayland` with the reason. The marker lives on a tmpfs, so direct is tried
  again after a reboot; delete it to retry sooner. The profile also falls
  back by itself after 3 headless starts that the daemon never confirmed.
  For an exit before confirmation, the reason carries the slicer's last
  non-empty stderr line (trimmed, capped at 300 characters) alongside the
  exit status — e.g. "the slicer exited before direct presentation was
  confirmed (exit status: 1): direct presentation needs
  `VK_EXT_present_timing` and `VK_KHR_present_id2`; this device/driver
  (NVIDIA ..., 550.163.01) exposes neither" — so an operator (or `/system`)
  never has to go find the actual cause in the journal.
- **Compositor loss.** If the headless Sway goes away (a crash, or
  `systemctl restart getty@tty1`), the direct-mode daemon stops its slicer
  and exits; systemd restarts it against whatever session comes up next.

## Recovery runbook

`suede display-reset` (`src/projection/gpu/display.rs`) opens every
`/dev/dri/card*`, and, on each one it identifies as `nvidia-drm`, calls
`clear_stale_sub_ownership` to revoke any NVKMS sub-ownership grant a crashed
direct-mode slicer left behind — the grant nvidia-drm otherwise rejects every
atomic commit against, including Sway's own, so the next compositor to start
would otherwise never get a picture up. It never fails the process: opening a
card, checking its driver, or revoking a grant can each fail independently
(most commonly because another process already holds DRM master, which makes
the revoke ioctl fail every time), and each is reported per card rather than
aborting the scan.

The slicer also installs a SIGTERM/SIGINT handler that reuses its own clean
shutdown path (`state.finish()` → `DirectDisplay::shutdown`'s NVKMS revoke),
so a graceful stop — including the cgroup SIGTERM systemd sends when the
daemon itself stops — already revokes the grant without needing
`display-reset` at all. Only a SIGKILL (or a harder crash) skips that path
and leaves a grant for `display-reset` to find later.

Recovery by scenario:

- **Slicer crash (SIGKILL or otherwise).** The manager respawns it. The new
  slicer's own startup clears any stale grant the old one left, so the
  respawn recovers on its own. Three exits within 10 minutes exhausts the
  crash budget and triggers fallback (below).
- **Daemon crash.** systemd restarts it (`Restart=always`). Its cgroup's
  SIGTERM already gave the slicer a clean exit before the daemon itself died,
  so there is usually nothing left to clear; if a SIGKILL raced it, the next
  slicer clears the leftover grant as above.
- **Headless Sway crash.** Auto-login on tty1 respawns it, up to 3 times per
  boot before the profile falls back to the ordinary DRM Sway itself. The
  direct-mode daemon, watching the compositor's IPC socket, exits when it
  goes away and restarts against whatever session comes up next.
- **Fallback (any trigger).** The daemon writes
  `$XDG_RUNTIME_DIR/suede/presentation-fallback`, stops the slicer
  gracefully (closes stdin, waits up to 10 s, then SIGKILL), runs
  `suede display-reset` as a bounded child, and asks the headless Sway to
  exit. The profile's own wait (up to 15 s) for the direct slicer to release
  DRM master runs before it starts the DRM Sway, so an operator restarting
  `getty@tty1` mid-session cannot race the ordinary compositor onto a card
  the slicer still holds. Read the marker's `reason` (or `GET /system`)
  before assuming another cause: for an exit before confirmation it already
  names the slicer's last stderr line, which is usually the actual failure —
  a missing capability, a denied DRM master, or similar — not just an exit
  status.
- **Reboot.** Runtime state lives on a tmpfs
  (`$XDG_RUNTIME_DIR/suede`), so a fallback marker and attempt counters do
  not survive it: direct presentation is retried on the next boot.
- **Manual recovery.** Set `presentation = "wayland"` in `suede.toml`, then
  `sudo systemctl restart getty@tty1` and `systemctl --user restart suede`.
  The profile runs `display-reset` itself whenever the previous session was
  `direct`, whether or not that session is the reason recovery was needed.

nvidia-drm reports success on a revoke whether or not a grant was actually
held, so neither `suede display-reset`'s "sub-ownership reset; any earlier
grant cleared" nor the slicer's own startup message claims a stale grant
existed — both print on any successful revoke, including right after an
already-clean one. Only the driver answering `EINVAL` — "no stale NVKMS
sub-ownership grant" — says the ioctl found nothing to do.

## GSP firmware

The `gsp-firmware` health check reads the live driver state straight from
`/proc/driver/nvidia` (see `src/nvidia_driver.rs`):
the loaded kernel module (proprietary or open), whether GSP firmware is
actually running for this GPU, and the driver version. `GET /api/v1/system`
reports it as `nvidiaDriver: { version, kernelModule, gspFirmware,
gspOptional, newestTested }`, or `null` on a machine with no NVIDIA driver
loaded at all.

- **Not an NVIDIA machine, GSP already off, or the open kernel module:**
  pass. The open module has no option to run without GSP, so there is
  nothing to recommend.
- **Driver does not report GSP state:** pass with that note. Older drivers
  (550 on [System B](test-systems.md#system-b)) print no `GPU Firmware:`
  line at all, so `gspFirmware` is `"unknown"`; those drivers lack the
  per-present timing direct presentation needs anyway, and
  `nvidia-driver-version` is the check that warns about them.
- **GSP on, proprietary module, effective Wayland:** pass — "GSP firmware
  on; no measured effect on the Wayland path". Its cost there is Sway's own
  EGL busy-wait, not per-present kernel objects (see
  [Conclusions](#conclusions-direct-vulkan-path-vs-swaywayland)).
- **GSP on, proprietary module, effective direct:** warn. The direct path
  pays about 12 points of one core in GSP round trips inside the present
  path (probe steady-state kernel CPU 17% → 4.6% for four heads, see
  [What's proven](#whats-proven-on-each-card)); disabling it needs
  `options nvidia NVreg_EnableGpuFirmware=0` in `/etc/modprobe.d/` and a
  reboot, and only the proprietary module supports it (verified on Turing
  and Ampere).
  `fixAvailable` is always false: writing a modprobe drop-in and rebooting
  the machine is outside what a session fix does without the operator's
  consent to reboot.

`nvidia-driver-version` is a separate check against the same detected
driver: it warns when the running version is older than `newestTested`
(`NEWEST_TESTED_NVIDIA_DRIVER` — see [What it takes to get
there](#what-it-takes-to-get-there)), since per-present timing needs driver
595 or later and older releases are not validated here either.

## Further reading

- [Direct-to-Display (`VK_KHR_display`) plan](../plans/VK_KHR_display.md)
- [Selectable display presentation component](../plans/display-integration.md)
- [Direct presentation component results](../plans/display-component-results.md)
- [System A](test-systems.md#system-a) [display baseline results](../plans/display-baseline-results.md)
