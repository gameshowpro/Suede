#!/usr/bin/env python3
"""Compare one output capture against the `coordinates` encoding.

Every canvas pixel (cx, cy) of Suede's `coordinates` test pattern (and of
coordinates.html in this directory) carries

    R = cx & 0xFF
    G = cy & 0xFF
    B = ((cx >> 8) & 0x0F) << 4 | ((cy >> 8) & 0x0F)

so a capture of an output showing a 1:1 slice whose top-left canvas pixel is
(ox, oy) must hold, at as-viewed capture pixel (x, y), the encoding of
(ox + x, oy + y). Anything else is a mismatch.

`grim -o <output>` delivers the output as viewed: it applies the output's
transform to the screencopy buffer itself. `--capture-space logical` (the
default) compares that directly. `--capture-space physical` is for a capture
that is still in buffer orientation; the capture is then mapped back to
logical with the same integer permutation as `Transform::logical_index` in
src/model/desired.rs before comparing.

On a mismatch the report also says, to help attribute it:
  * the signed per-channel delta histogram (captured minus expected);
  * where the mismatches are (bounding box, rows and columns touched,
    per-quadrant counts, mean delta by expected channel value in 16 bins);
  * the canvas coordinate each mismatched pixel decodes to, as an offset
    histogram (a constant offset means a shifted slice, not lossy pixels);
  * whether the capture matches exactly under some other transform (a
    chirality or orientation error rather than a value error).

Prints one PASS/FAIL line, writes the full report as JSON with --json, and
exits 0 on zero mismatches, 1 on any mismatch, 2 on a usage or input error.

Needs numpy and Pillow. Run it with `python -I`.
"""

import argparse
import json
import sys

import numpy as np
from PIL import Image

TRANSFORMS = (
    "normal",
    "90",
    "180",
    "270",
    "flipped",
    "flipped-90",
    "flipped-180",
    "flipped-270",
)
SWAPS = {"90", "270", "flipped-90", "flipped-270"}


def logical_index(transform, px, py, w, h):
    """`Transform::logical_index`: physical index -> logical index.

    `w`, `h` are the logical raster size. Works elementwise on arrays.
    """
    if transform == "normal":
        return px, py
    if transform == "90":
        return w - 1 - py, px
    if transform == "180":
        return w - 1 - px, h - 1 - py
    if transform == "270":
        return py, h - 1 - px
    if transform == "flipped":
        return w - 1 - px, py
    if transform == "flipped-90":
        return py, px
    if transform == "flipped-180":
        return px, h - 1 - py
    if transform == "flipped-270":
        return w - 1 - py, h - 1 - px
    raise ValueError(f"unknown transform {transform!r}")


def physical_to_logical(image, transform):
    """Re-index a physical (buffer-orientation) HxWx3 image to logical."""
    ph, pw = image.shape[:2]
    w, h = (ph, pw) if transform in SWAPS else (pw, ph)
    py, px = np.mgrid[0:ph, 0:pw]
    lx, ly = logical_index(transform, px, py, w, h)
    out = np.empty((h, w, 3), dtype=image.dtype)
    out[ly, lx] = image[py, px]
    return out


def encode(ox, oy, w, h):
    """The `coordinates` encoding of the w x h canvas rect at (ox, oy)."""
    cy, cx = np.mgrid[oy : oy + h, ox : ox + w]
    out = np.empty((h, w, 3), dtype=np.uint8)
    out[..., 0] = cx & 0xFF
    out[..., 1] = cy & 0xFF
    out[..., 2] = (((cx >> 8) & 0x0F) << 4) | ((cy >> 8) & 0x0F)
    return out


def decode(image, ox, oy):
    """Canvas coordinates each pixel decodes to, choosing the 4096-periodic
    candidate nearest the expected origin."""
    r = image[..., 0].astype(np.int64)
    g = image[..., 1].astype(np.int64)
    b = image[..., 2].astype(np.int64)
    cx = r | (((b >> 4) & 0x0F) << 8)
    cy = g | ((b & 0x0F) << 8)
    # Unwrap against the expected position so an origin past 4096 decodes
    # to a small offset rather than a 4096 jump.
    h, w = image.shape[:2]
    ey, ex = np.mgrid[oy : oy + h, ox : ox + w]
    cx = cx + 4096 * np.round((ex - cx) / 4096).astype(np.int64)
    cy = cy + 4096 * np.round((ey - cy) / 4096).astype(np.int64)
    return cx, cy


def top_counts(values, limit):
    keys, counts = np.unique(values, return_counts=True)
    order = np.argsort(-counts, kind="stable")[:limit]
    return [[int(keys[i]), int(counts[i])] for i in order]


def describe(captured, expected, ox, oy):
    """Everything the report says about a mismatching capture."""
    h, w = expected.shape[:2]
    cap = captured.astype(np.int16)
    exp = expected.astype(np.int16)
    delta = cap - exp
    bad = np.any(delta != 0, axis=2)
    count = int(bad.sum())
    report = {"mismatched": count, "pixels": w * h}
    if count == 0:
        return report

    ys, xs = np.nonzero(bad)
    first = (int(ys[0]), int(xs[0]))  # row-major: topmost, then leftmost
    report["first"] = {
        "x": first[1],
        "y": first[0],
        "canvas": [ox + first[1], oy + first[0]],
        "expected": [int(v) for v in expected[first]],
        "captured": [int(v) for v in captured[first]],
    }

    histograms = {}
    by_value = {}
    for index, channel in enumerate("RGB"):
        d = delta[..., index][bad]
        nonzero = d[d != 0]
        histograms[channel] = {
            "pixelsWithDelta": int(nonzero.size),
            "min": int(nonzero.min()) if nonzero.size else 0,
            "max": int(nonzero.max()) if nonzero.size else 0,
            "counts": top_counts(nonzero, 32) if nonzero.size else [],
        }
        # Mean delta against the expected channel value, in 16 bins: a
        # value-dependent curve points at a color conversion, a flat one
        # at an offset or a dither.
        ev = expected[..., index][bad].astype(np.int64)
        dv = delta[..., index][bad].astype(np.int64)
        bins = []
        for start in range(0, 256, 16):
            sel = (ev >= start) & (ev < start + 16)
            n = int(sel.sum())
            bins.append(
                {
                    "expected": [start, start + 15],
                    "pixels": n,
                    "meanDelta": round(float(dv[sel].mean()), 3) if n else None,
                    "nonzero": int((dv[sel] != 0).sum()),
                }
            )
        by_value[channel] = bins
    report["channelDeltas"] = histograms
    report["meanDeltaByExpectedValue"] = by_value

    magnitude = np.abs(delta).max(axis=2)[bad]
    report["maxAbsDelta"] = top_counts(magnitude, 16)

    report["spatial"] = {
        "bbox": {
            "x0": int(xs.min()),
            "y0": int(ys.min()),
            "x1": int(xs.max()),
            "y1": int(ys.max()),
        },
        "rowsTouched": int(np.unique(ys).size),
        "columnsTouched": int(np.unique(xs).size),
        "quadrants": {
            "topLeft": int(bad[: h // 2, : w // 2].sum()),
            "topRight": int(bad[: h // 2, w // 2 :].sum()),
            "bottomLeft": int(bad[h // 2 :, : w // 2].sum()),
            "bottomRight": int(bad[h // 2 :, w // 2 :].sum()),
        },
        "edgeRowsAndColumns": int(
            bad[0, :].sum() + bad[-1, :].sum() + bad[:, 0].sum() + bad[:, -1].sum()
        ),
    }

    cx, cy = decode(captured, ox, oy)
    ey, ex = np.mgrid[oy : oy + h, ox : ox + w]
    offsets = np.stack([(cx - ex)[bad], (cy - ey)[bad]], axis=1)
    keys, counts = np.unique(offsets, axis=0, return_counts=True)
    order = np.argsort(-counts, kind="stable")[:8]
    report["decodedOffsets"] = [
        {"dx": int(keys[i][0]), "dy": int(keys[i][1]), "pixels": int(counts[i])}
        for i in order
    ]
    return report


def other_transforms(captured, ox, oy):
    """Transforms under which the capture would match exactly.

    The capture is treated as if it were a physical buffer under each
    candidate and re-indexed to logical; a zero-mismatch hit means the
    content is right but oriented by that permutation.
    """
    hits = []
    for t in TRANSFORMS:
        if t == "normal":
            continue
        view = physical_to_logical(captured, t)
        vh, vw = view.shape[:2]
        if np.array_equal(view, encode(ox, oy, vw, vh)):
            hits.append(t)
    return hits


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("capture", help="PNG from grim -o <output>")
    parser.add_argument("--name", required=True, help="output name, for the report")
    parser.add_argument("--label", default="", help="run label, for the report")
    parser.add_argument("--origin", type=int, nargs=2, metavar=("X", "Y"), required=True,
                        help="canvas pixel at the slice's top-left")
    parser.add_argument("--size", type=int, nargs=2, metavar=("W", "H"), required=True,
                        help="logical (as-viewed) raster of the output")
    parser.add_argument("--transform", default="normal", choices=TRANSFORMS,
                        help="the output's configured transform")
    parser.add_argument("--capture-space", default="logical", choices=("logical", "physical"),
                        help="orientation of the capture (grim delivers logical)")
    parser.add_argument("--json", help="write the full report here")
    args = parser.parse_args()

    try:
        with Image.open(args.capture) as img:
            mode = img.mode
            rgba = np.asarray(img.convert("RGBA"))
    except OSError as error:
        print(f"ERROR {args.name}: cannot read {args.capture}: {error}", file=sys.stderr)
        return 2
    captured = np.ascontiguousarray(rgba[..., :3])
    alpha_not_opaque = int((rgba[..., 3] != 255).sum())

    if args.capture_space == "physical":
        captured = physical_to_logical(captured, args.transform)

    w, h = args.size
    ox, oy = args.origin
    report = {
        "name": args.name,
        "label": args.label,
        "transform": args.transform,
        "captureSpace": args.capture_space,
        "captureMode": mode,
        "captureSize": [int(captured.shape[1]), int(captured.shape[0])],
        "expectedSize": [w, h],
        "origin": [ox, oy],
        "alphaNotOpaque": alpha_not_opaque,
    }

    if captured.shape[:2] != (h, w):
        report["error"] = "capture size differs from the output's logical raster"
        line = (f"FAIL {args.name} ({args.transform}) {args.label}: capture is "
                f"{captured.shape[1]}x{captured.shape[0]}, expected {w}x{h}")
        status = 1
    else:
        expected = encode(ox, oy, w, h)
        report.update(describe(captured, expected, ox, oy))
        n = report["mismatched"]
        if n == 0:
            line = f"PASS {args.name} ({args.transform}) {args.label}: 0/{w * h} pixels mismatched"
            status = 0
        else:
            report["matchesUnderTransform"] = other_transforms(captured, ox, oy)
            f = report["first"]
            hist = "; ".join(
                f"{c} " + (", ".join(f"{d:+d}x{k}" for d, k in report["channelDeltas"][c]["counts"][:6])
                           or "none")
                for c in "RGB"
            )
            line = (f"FAIL {args.name} ({args.transform}) {args.label}: {n}/{w * h} pixels mismatched; "
                    f"first at ({f['x']},{f['y']}) canvas ({f['canvas'][0]},{f['canvas'][1]}) "
                    f"expected {tuple(f['expected'])} got {tuple(f['captured'])}; "
                    f"deltas {hist}")
            if report["matchesUnderTransform"]:
                line += f"; matches exactly as if transformed {report['matchesUnderTransform']}"
            status = 1

    if args.json:
        with open(args.json, "w", encoding="utf-8") as out:
            json.dump(report, out, indent=1)
    print(line)
    return status


if __name__ == "__main__":
    sys.exit(main())
