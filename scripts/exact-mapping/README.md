# End-to-end exactness harness

Checks on a real appliance that a 1:1 slice reaches every output pixel for
pixel, including outputs whose transform the slicer owns (180 and the rest).
The GPU readback test `exact_copy_is_byte_exact_under_every_transform` proves
the shader; this proves the whole path: canvas, capture, slicer, compositor.

| File | Role |
|---|---|
| `../../scripts/verify-exact-mapping.sh` | Runs everything over `ssh <host>` against the daemon's local API |
| `compare_capture.py` | Compares one capture with the `coordinates` encoding (numpy and Pillow, run with `python -I`) |
| `coordinates.html` | The browser variant's page: the same encoding drawn with `<canvas>` and `putImageData` |

## The encoding

Every canvas pixel `(x, y)` is `R = x & 0xFF`, `G = y & 0xFF`,
`B = ((x >> 8) & 0x0F) << 4 | ((y >> 8) & 0x0F)`, unique up to 4096×4096. A
capture of an output whose 1:1 slice starts at canvas pixel `(ox, oy)` must
hold the encoding of `(ox + x, oy + y)` at as-viewed pixel `(x, y)`. Because
the encoding is invertible, a mismatching pixel also says which canvas pixel
it came from.

## Running it

```bash
EXACT_PYTHON='~/venv-with-numpy/bin/python' scripts/verify-exact-mapping.sh <host>
```

The host needs `jq`, `grim`, `python3` and a Python with numpy and Pillow
(`EXACT_PYTHON`; a leading `~` expands on the host). The comparison runs on the
CPU. Other knobs, all optional:

| Variable | Meaning |
|---|---|
| `EXACT_VARIANTS` | `static browser` (default), or one of them |
| `EXACT_BROWSER_FLAGS` | Flag sets from `a b c d` (default `a b c`, plus `d` automatically when all three mismatch) |
| `EXACT_OUT` | Local directory to copy the JSON reports, and every mismatching capture, into |

Preconditions, checked before anything changes (exit 2 if one fails): the
saved document is what is live (`committed: true`, no highlight working copy),
it has a `projection.canvas` and a `projection.arrangement`, the arrangement
solved at content scale 1 (`GET /api/v1/projection/arrangement?...&contentScale=1`,
a dry run) gives every enabled output an integral pixel slice exactly the size
of its as-viewed raster at output scale 1, and every one of those outputs is
connected and capturable by `grim`.

### Static variant

One uncommitted working copy of the saved document with `projection.mode =
simple`, `blend = false`, `blackLift = 0`, `testPattern = coordinates`, and
each output's `geometry.slice` set to its 1:1 rect from the dry run (warp pins
stay in the document; Simple ignores them). The harness waits until
`GET /api/v1/projection/stats` shows `control.configGeneration.applied` at or
past the working copy's `X-Config-Generation`, `samplingMode: exact` on every
output, and each output's `presentedGeneration` at or past
`control.childGeneration.applied`. Then it runs `grim -o <output>` per output
and compares.

### Browser variant

`coordinates.html` is served from a scratch directory by `python3 -m
http.server` bound to `127.0.0.1` on the host. Each flag set gets its own
temporary `chromium-kiosk` app (`exact-mapping-probe-<set>`), added **and
activated inside the uncommitted working copy** (`apps += [...]`, `activeApp`
set to it, `testPattern` absent). That is deliberate: there is no
`POST /api/v1/apps`, and `POST /api/v1/apps/{id}/activate` commits, which this
harness must never do. The page reports its viewport after each draw by
requesting `/report?w=...&h=...&dpr=...` (a 404 the server's log records); the
harness warns when the viewport is not the canvas size at `devicePixelRatio`
1, then waits as for the static variant, settles 5 s, captures and compares.

| Set | `extraArgs` |
|---|---|
| a | none: the launcher preset as is |
| b | `--force-color-profile=srgb` |
| c | `--disable-gpu-rasterization` |
| d | `--disable-gpu-compositing`, a diagnostic run only when a, b and c all mismatch |
| e | `--disable-accelerated-2d-canvas`, a diagnostic: is a mismatch specific to `<canvas>`? |

### Restore

Everything the harness changes is uncommitted. An `EXIT` trap (also reached on
`INT`, `TERM`, `HUP` and `PIPE`) calls `POST /api/v1/config/revert`, stops the
http server, waits for the original `activeApp` to return to its earlier
state, removes the temporary apps' browser profiles (only ones that did not
exist before the run), deletes the scratch directory, and then asserts that
`GET /api/v1/config` is byte-identical to the read at the start, that the
`GET /api/v1/apps` id list is unchanged, and that `activeApp` is unchanged,
printing `RESTORE OK` or `RESTORE FAILED` for each.

## Reading the result

One line per output and run, for example:

```
PASS DP-3 (normal) static: 0/2073600 pixels mismatched
FAIL DP-1 (180) browser-a: 11/2073600 pixels mismatched; first at (7,5) canvas (1927,1085) expected (135, 61, 116) got (135, 62, 116); deltas R -2x10; G +1x1; B none
```

The JSON report next to each capture adds the full signed delta histogram per
channel, the mean delta against the expected channel value in 16 bins (a
value-dependent curve points at a color conversion, a flat one at an offset or
a dither), the bounding box, rows, columns and quadrants touched, the decoded
canvas offset of the mismatching pixels (a single constant offset is a shifted
slice, not lossy pixels), and whether the capture matches exactly under some
other transform (an orientation error rather than a value error).

Exit status: 0 only when every run matched on every output and the restore
verified, 1 otherwise, 2 when a precondition stopped the run first.

## What a mismatch can and cannot blame

`grim` reads each output through the compositor's screencopy, not the
scanout buffer, and applies the output transform itself (`compare_capture.py
--capture-space logical`, the default). A capture can therefore be lossy on
its own. The harness compares normal-transform outputs first, as the control:
if one of them mismatches, it prints `CONTROL FAILED`, reports the rotated
outputs' mismatches as `INCONCLUSIVE` rather than `FAIL`, and the run says
nothing about the slicer. Only a clean control makes a rotated output's
mismatch the slicer's. Scanout itself stays a trust boundary Suede cannot read
back.

The static pattern is uploaded into the slicer directly. It proves the slicer,
the compositor and the capture of each output, but it never crosses the
slicer's capture of the source output. So each browser run also captures the
source output itself (the compositor's headless output the browser renders
into) and compares it with the whole canvas, printed as a `SOURCE` line. That
line is context and does not change the exit status. A mismatch there is
upstream of Suede: in the browser or in the compositor's composition of the
browser's surface.

`lastInterval.outputs[].zeroCopyPresented/presented` is printed for each run
as context; it is not part of any verdict.

## Results

### System A, 2026-10-30

Build `v0.4.0-7-g976bbd0`, four 1920×1080 heads (DP-1 and DP-2 at transform
180, DP-3 and DP-4 normal) on a 3840×2160 canvas in a 2×2 arrangement at
content scale 1, all reporting `samplingMode: exact`, all presenting zero-copy
(8/8 per interval). Eight harness runs. Every run restored the saved document
byte-identical, with the app list and `activeApp` unchanged.

**Static variant: exact.** Zero mismatches on all four outputs, including the
two 180 outputs, in both runs that included it.

**Browser variant: exact except one canvas column, which the browser gets
wrong before Suede sees it.** Every mismatch was the same: canvas column
x = 2048, every row, red one too high (encoded 0, captured 1). That is the red
value of x = 2049 with the blue of x = 2048. Green and blue were exact, and no
other column was wrong. Other columns with red 0 (x = 1792, 2304, ...) were
exact, so this is spatial, not a value conversion. It appears on DP-4 and DP-2,
the two outputs whose slices contain x = 2048, at local column 128.

| Flag set | Runs | Runs with the x = 2048 column |
|---|---|---|
| a (launcher preset) | 8 | 6 |
| b (`--force-color-profile=srgb`) | 1 | 1 |
| c (`--disable-gpu-rasterization`) | 4 | 3 |
| e (`--disable-accelerated-2d-canvas`) | 1 | 0 |

It is intermittent under every flag set. No flag set tested prevents it,
including software rasterization. The single clean run for e is not evidence
that e fixes it, and a passed twice and c once. In the three runs that also
captured the source output, HEADLESS-1 itself carried the same column (2160
pixels, red +1, nothing else). The fault is therefore upstream of Suede's
capture, in the browser or in the compositor's composition of the browser's
surface. The slicer copied what it was given byte for byte. Telling the browser
from the compositor needs a capture from inside the browser (for example a
DevTools `Page.captureScreenshot`), which this harness does not take.

In five of the eight runs, the last viewport report the page sent before
capture was 3840×2073, not 3840×2160, after a third draw. The captures were
still full-canvas and exact in every other row and column, so the reports
probably arrive out of order. The harness only warns about it.
