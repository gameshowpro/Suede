# Black Offset (Luminance Lift / Floor Compensation) Architecture & Implementation Plan

Status:
- **Phase 1 ("off" / "constant")**: **IMPLEMENTED & VALIDATED** (All core math, topology detection, forced "off", and transfer tables verified in automated test suite).
- **Phase 2 ("dynamic" 2D LUT)**: Proposed architectural design.
- **Phase 3 ("time domain")**: Proposed architectural design.

Context: Extends and refines the projection warping pipeline documented in [How It Works](../how-it-works.md#warping) and the [Configuration Reference](../configuration.md#projection-geometry).

---

## 1. Executive Summary & Physical Problem Formulation

### 1.1 The Physical Problem: Optical Black Leakage in Multi-Projector Overlaps

Projection displays cannot subtract light; their native contrast ratio dictates an inherent optical black leakage floor $L_{\text{black}} > 0$. When displaying pure digital black $(0, 0, 0)$, a projector still emits physical light.

In multi-projector installations with overlapping beams (seams in multi-column/row arrays, 4-corner grid intersections, or multi-projector stacks), this optical leakage is purely additive on the physical screen:

$$L_{\text{screen}}(x, y) = \sum_{i \in \text{active}} \mathbb{I}_i(x, y) \cdot L_{\text{black}, i}$$

Where $\mathbb{I}_i(x, y)$ is 1 if output $i$'s light footprint covers physical coordinate $(x, y)$, and 0 otherwise.

In an $N \times M$ blended projection array:
- **Single-coverage areas** ($k = 1$ projector) receive $1 \times L_{\text{black}}$.
- **Edge-blend seams** ($k = 2$ projectors) receive $2 \times L_{\text{black}}$.
- **Grid corners** ($k = 4$ projectors) receive $4 \times L_{\text{black}}$.

During dark scenes or pure black backgrounds, this creates prominent, distracting bright steps and bands along the overlap zones. Edge blending ramps only solve the gain roll-off for non-black pixels; they cannot attenuate the native optical black floor.

```
Physical Screen Luminance at Digital Black (0, 0, 0) without Compensation:

Single (k=1)      Seam (k=2)       Single (k=1)
 [ 1x L_black ] | [ 2x L_black ] | [ 1x L_black ]
                ^                ^
                Visible brighter seam banding
```

### 1.2 The Solution: Black Offset (Luminance Lift)

To eliminate the visible seam bands, the black floor of less-overlapped regions must be optically elevated ("lifted" or "offset") so that the entire physical projection surface reaches a uniform black level matching the maximum overlap zone:

$$L_{\text{target\_black}} = N_{\max} \cdot L_{\text{black}}$$

Where $N_{\max} = \max_{(x,y)} k(x,y)$ is the maximum overlap count across the projection topology.

### 1.3 Phased Evolution Overview

| Phase | Mode | Core Mechanism | Dimensionality & Storage | Status |
|---|---|---|---|---|
| **Phase 1** | `"off"`, `"constant"` | Static compensation: detects $N_{\max}$, lifts less-overlapped areas via projector gamma; forced `"off"` when $N_{\max} \le 1$. | 1D transfer per output pixel (fixed $(a, b)$ fixed-point table). | **IMPLEMENTED & VALIDATED** |
| **Phase 2** | `"dynamic"` (Content-Luminance Dependent) | Non-linear roll-off: full lift in deep shadows, smoothly tapering to zero as local pixel luminance rises. | **2D transfer dimension**: Signal Level $\times$ Overlap Deficit ($256 \times 256$ LUT or parametric shader). | Proposed Design |
| **Phase 3** | Time Domain (Scene-Adaptive) | Temporal contrast masking: modulates global lift based on frame luminance and eye adaptation, with asymmetric attack/release. | Real-time GPU luminance sensing + temporal low-pass filter with slew limiting. | Proposed Design |

---

## 2. Phase 1: Constant and Off Modes

### 2.1 Configuration Schema

The configuration replaces/unifies the legacy `blackLift` property with `blackOffset`:

```json
"projection": {
  "mode": "warp",
  "blackOffset": {
    "mode": "constant",
    "amount": 0.04
  },
  "gamma": 2.2
}
```

#### Field Specifications:
- `mode`: String enum: `"off"` | `"constant"` (Phases 2 and 3 add `"dynamic"` and `"adaptive"`).
- `amount`: Number in $[0.0, 1.0]$ (recommended default `0.0`, default limit `0.5`). Governs the strength of the lift compensation.
- **Backwards Compatibility**: A bare numeric `blackLift: 0.04` or object `blackLift: {"mode": "fixed", "level": 0.04}` is deserialized into `blackOffset: {"mode": "constant", "amount": 0.04}` with identical runtime semantics.

### 2.2 Forced "Off" Topology Rule

**Rule**: If the active projection layout contains no overlapping projectors ($N_{\max} \le 1$), the black offset mode is **strictly forced to `"off"`**.

- **Evaluation**: The topology engine inspects the roster of active enabled outputs and their calibrated footprints/sources. It computes:
  $$N_{\max} = \max_{(x, y) \in \text{canvas}} k(x, y)$$
- **Enforcement**: If $N_{\max} \le 1$ (e.g. single output, independent tiled video walls with zero overlap, or disconnected topology):
  - The effective runtime mode is set to `"off"`.
  - The shader pipeline bypasses all lift logic ($b = 0$).
  - `GET /api/v1/projection/stats` reports:
    ```json
    "blackOffset": {
      "requestedMode": "constant",
      "effectiveMode": "off",
      "reason": "no-overlaps",
      "maxOverlap": 1
    }
    ```
- **Rationale**: Raising the black floor on a non-overlapping display has zero perceptual or blending benefit and strictly degrades display contrast ratio.

### 2.3 Constant Mode Transfer Mathematics

When $N_{\max} \ge 2$, any physical point covered by $k$ projectors ($1 \le k \le N_{\max}$) has an overlap deficit:

$$\Delta k(x, y) = N_{\max} - k(x, y)$$

- If $k = N_{\max}$ (maximum overlap area): Deficit is 0. No lift is applied.
- If $k < N_{\max}$: The area lacks the light of $(N_{\max} - k)$ projectors.
- Because all $k$ projectors lighting that point contribute to the screen, each of the $k$ projectors must supply an equal share of the missing optical light:

$$\text{Share per projector} = \frac{N_{\max} - k}{k}$$

#### Gamma Curve Compensation
Projector panels follow an electro-optical transfer function (EOTF) approximately modeled by $L = V^\gamma$, where $\gamma$ is the configured display gamma (typically 2.2).

To produce a linear optical lift proportion $\Delta L_{\text{norm}}$ relative to full white:
$$\Delta L_{\text{norm}}(k) = \text{amount} \cdot \frac{N_{\max} - k}{k \cdot N_{\max}}$$

The required digital drive signal offset $b \in [0, 1]$ before scaling into 8-bit integer space is:
$$b(k) = \left( \Delta L_{\text{norm}}(k) \right)^{1/\gamma}$$

Alternatively, using Suede's linear transfer arithmetic:
$$y_{\text{linear}} = a \cdot x_{\text{linear}} + b_{\text{linear}}$$
Where:
- $b_{\text{linear}} = \text{amount} \cdot \frac{N_{\max} - k}{k}$
- Edge border antialiasing coverage factor $c \in [0, 1]$ attenuates both gain and offset:
  $$a = \text{round}\left(c \cdot (1 - \text{lift}) \cdot r \cdot 256\right)$$
  $$b = \text{round}\left(c \cdot \text{lift} \cdot 255\right)$$

### 2.4 Data Structures & Storage (Phase 1)
- Fits directly into Suede's existing 1D precalculated per-output transfer table:
  - Stored as a packed 32-bit integer per output pixel: `(u32::from(a) << 8) | u32::from(b)`.
  - Applied in a single pass in the output fragment shader:
    $$\text{out} = \min\left( \left( (a \cdot \text{in}) \gg 8 \right) + b, 255 \right)$$

### 2.5 Validation & Test Evidence (Status: VERIFIED)

Phase 1 is fully implemented in Suede's core pipeline (`Coverage::lift` and `pixel_transfer` in `src/projection/blend.rs`) and passes all automated unit tests:

1. **Forced "Off" on Non-Overlapping Topologies**:
   * Verified by `test projection::blend::tests::non_overlapping_topology_forces_lift_to_zero`.
   * Proves that single-display and tiled non-overlapping arrays ($N_{\max} = 1$) return strictly `0.0` lift everywhere, preventing black lift from degrading contrast on non-blended screens.
2. **Proportional Multi-Projector Shortfall Lift**:
   * Verified by `test projection::blend::tests::a_grid_lifts_every_region_by_its_own_shortfall`.
   * In a $2 \times 2$ grid with $N_{\max} = 4$: a lone projector ($k=1$) receives 3 shares ($0.15$), a 2-way seam ($k=2$) receives 1 share ($0.05$), and the 4-way center ($k=4$) receives $0.0$.
3. **Optical Black Floor Uniformity**:
   * Verified by `test projection::blend::tests::total_black_is_even_across_a_grid`.
   * Proves that total emitted optical black $n \cdot (1 + \text{lift} / L)$ equals $4.0$ black units identically across single areas, vertical seams, horizontal seams, and center intersections.
4. **Gamma-Corrected Transfer Table Integration**:
   * Verified by `test projection::blend::tests::black_lift_applies_outside_seams_only`.
   * Tests $(a, b)$ fixed-point packing and border attenuation with gamma shaping.

---

## 3. Phase 2: Dynamic Mode (Content-Luminance Dependent without Masks)

### 3.1 Motivation & Concept

In Constant mode, adding a static offset $b$ lifts the black floor across all input pixel values. In a scene that contains mixed shadows and highlights, this:
1. Compresses native dynamic range.
2. Creates a washed-out, milky appearance in dark shadows.
3. Wastefully raises pixels that already have sufficient light to overpower the dark seam.

The **Dynamic Black Offset without masks** principle addresses this:
- It **intelligently increases the brightness of non-overlapping areas to visually match the overlapped regions during dark scenes**.
- It **preserves the high dynamic range when projecting brighter content** by smoothly rolling off the black offset to zero as pixel luminance increases.

```
Luminance Transfer Curves:
Signal Out
 ^
1.0 |                                      / (Native / Full White)
    |                                    /
    |                                  /
    |                        . - - - /
    |                     /  Dynamic Roll-off (Smooth Knee)
 b  |------+------------/
    |      |          /
    |      | Constant Lift (Constant mode raises floor everywhere)
0.0 +------+-----------------------------------> Signal In
    0    Y_thresh                            1.0
```

### 3.2 Dynamic Roll-Off Curve Formulation

Let $V_{\text{in}} \in [0, 1]$ be the input color channel value, and $Y_{\text{in}} \in [0, 1]$ be the perceptual input luminance:
$$Y_{\text{in}} = 0.2126\, R_{\text{in}} + 0.7152\, G_{\text{in}} + 0.0722\, B_{\text{in}}$$

We define a continuous roll-off modulation function $g(Y_{\text{in}}) \in [0, 1]$ governed by a knee threshold $Y_{\text{thresh}}$ (default $0.15$):

$$g(Y_{\text{in}}) = \begin{cases}
1.0 - 3\left(\frac{Y_{\text{in}}}{Y_{\text{thresh}}}\right)^2 + 2\left(\frac{Y_{\text{in}}}{Y_{\text{thresh}}}\right)^3 & \text{for } 0 \le Y_{\text{in}} < Y_{\text{thresh}} \quad (\text{Hermite smoothstep}) \\
0.0 & \text{for } Y_{\text{in}} \ge Y_{\text{thresh}}
\end{cases}$$

The dynamic output transfer becomes:
$$V_{\text{out}}(u, v) = a(u, v) \cdot V_{\text{in}} + b(u, v) \cdot g(Y_{\text{in}})$$

When $Y_{\text{in}} = 0$ (black), $g(0) = 1.0 \implies V_{\text{out}} = b(u, v)$ (identical to Constant mode, eliminating the dark seam).
When $Y_{\text{in}} \ge Y_{\text{thresh}}$, $g(Y_{\text{in}}) = 0.0 \implies V_{\text{out}} = a(u, v) \cdot V_{\text{in}}$ (full native contrast, zero wash-out).

### 3.3 Adding an Extra Dimension to Pre-Calculated Tables

In Phase 1, the transfer function is a single affine transformation $(a, b)$ per output pixel (1 dimension: spatial pixel index).
In Phase 2, the transfer function varies non-linearly with **two independent parameters**:
1. **Spatial Overlap State**: The pixel's position / seam weight / overlap deficit: $w(u, v) \in [0, 1]$.
2. **Input Signal Level**: The incoming pixel value: $x \in [0, 255]$.

#### Evaluated Architectural Options:

| Architecture | Description | Memory Footprint | GPU ALU Cost | Hardware Compatibility | Recommended? |
|---|---|---|---|---|---|
| **Option A: 2D Texture LUT** | A global 2D lookup table: `LUT[signal_in, overlap_weight]`. Shader samples `texture(u_lut2d, vec2(in_val, weight))`. | $256 \times 256$ texels = 64 KB (R8) or 128 KB (R16). | Extremely low (1 texture fetch per color channel). | Universal (Vulkan, GLES 3.0, Pi 5 V3D). | **Yes (Primary)** |
| **Option B: 3D Texture / Per-Pixel LUT Array** | A 256-entry table for every output pixel. | $1920 \times 1080 \times 256 \times 1\text{B} \approx 530\text{ MB}$ per display. | Zero ALU. | Unacceptable memory footprint. | **No** |
| **Option C: Parametric Shader Evaluation** | Spatial map stores $(a, b_{\max})$. Shader computes polynomial roll-off $g(Y)$ dynamically in ALU. | Zero extra memory (reuses Phase 1 textures). | ~15 ALU cycles per fragment. | High, allows live $Y_{\text{thresh}}$ changes without rebuilding tables. | **Yes (Alternative / Push Constant path)** |

#### Recommended Implementation: 2D Texture LUT (Option A)
1. **Host Table Generator**:
   - Generates a $256 \times 256$ 2D transfer texture `u_lut2d`.
   - Dimension $X$ ($0..255$): Input signal level.
   - Dimension $Y$ ($0..255$): Normalized overlap weight / deficit ($0 = N_{\max}$ overlap, $255 = \text{single coverage}$).
   - Entry value: Precomputed output value including display gamma curve and Hermite roll-off:
     $$\text{LUT}(x, y) = \text{round}\left( 255 \cdot \left( \left( (1 - b(y)) \cdot \left(\frac{x}{255}\right)^\gamma + b(y) \cdot g\left(\frac{x}{255}\right) \right)^{1/\gamma} \right) \right)$$
2. **Output Fragment Shader**:
   ```glsl
   // Sample input color from browser canvas
   vec4 src = texture(u_canvas, uv);
   
   // Retrieve spatial overlap deficit weight from geometry map
   float weight = texture(u_weight_map, out_coord).r;
   
   // Apply 2D LUT transfer across R, G, B channels
   vec3 out_rgb;
   out_rgb.r = texture(u_lut2d, vec2(src.r, weight)).r;
   out_rgb.g = texture(u_lut2d, vec2(src.g, weight)).r;
   out_rgb.b = texture(u_lut2d, vec2(src.b, weight)).r;
   ```
3. **Benefits**:
   - Only 64 KB VRAM per appliance.
   - Bilinear hardware filtering provides perfectly smooth transitions across both the signal domain and spatial seam edges.
   - Zero banding artifacts.

---

## 4. Phase 3: Time Domain (Scene-Adaptive Contrast & Temporal Smoothing)

### 4.1 Motivation: Human Vision & Simultaneous Contrast Masking

Human visual perception does not perceive absolute luminance linearly ($Weber-Fechner$ law). When viewing a projection surface:
- In a **dark scene** (e.g. night sky, dark room), human pupils dilate and the visual system adapts to scotopic/mesopic vision. Differences of $0.01\text{ cd/m}^2$ at the seam are immediately noticeable. Dynamic black offset is essential here.
- In a **bright scene** (e.g. outdoor daylight, white background, bright video), pupils constrict and the eye's threshold for dark contrast increases by several orders of magnitude (**simultaneous contrast masking**). The native black leakage at the seam is completely invisible to human observers because the surrounding field is thousands of times brighter.
- **Problem**: Leaving black offset active during bright scenes unnecessarily degrades deep shadow detail in darker portions of the frame.
- **Solution**: Add the **time domain** to modulate the global black offset strength based on measured scene luminance over time.

```
Temporal Scene Modulation Factor M(t):

Scene Luminance (Y_scene)
  ^
1.0 |                /-----------------\
    |               /                   \
0.0 +--------------/                     \-----------------> Time
    
Offset Multiplier M(t)
1.0 | ------------\                       /-----------------
    |              \                     /
0.0 +               \-------------------/                  -> Time
      (Dark Scene:   (Bright Scene:      (Dark Scene:
       M=1.0 Full)    M=0.0 Extinguished) M=1.0 Recovered)
```

### 4.2 Real-Time Scene Luminance Sensing

1. **Measurement Domain**:
   - Sample the authoritative virtual canvas before ramps, geometry warping, or output slicing.
   - Compute mean linear luminance:
     $$Y_{\text{scene}} = \frac{1}{|\Omega|} \sum_{(x,y) \in \Omega} \left( 0.2126\, R_{\text{lin}} + 0.7152\, G_{\text{lin}} + 0.0722\, B_{\text{lin}} \right)$$
2. **GPU Reduction Implementation**:
   - A $256 \times 256$ regular grid reduction pass scheduled once per newly captured browser frame.
   - Avoids measuring post-lift output (which would cause feedback instability).
   - Read back through a non-blocking GPU query/buffer, pipelined behind fence completion.

### 4.3 Temporal Filter & Envelope Follower

The raw scene luminance $Y_{\text{scene}}$ is converted to a target modulation factor $T \in [0.0, 1.0]$:

$$T = \text{clamp}\left( \frac{Y_{\text{bright}} - Y_{\text{scene}}}{Y_{\text{bright}} - Y_{\text{dark}}}, 0.0, 1.0 \right)$$

Where default thresholds are $Y_{\text{dark}} = 0.02$ and $Y_{\text{bright}} = 0.20$.

#### Asymmetric Temporal Attack and Release
Human vision adapts to sudden brightness much faster than to darkness. Therefore, the temporal controller must employ asymmetric smoothing:
- **Fast Attack ($\tau_{\text{attack}} \approx 50\text{--}100\text{ ms}$)**: When a scene suddenly cuts from dark to bright ($T$ decreases), the black offset must drop immediately to prevent a visible washed-out flash.
- **Slow Release ($\tau_{\text{release}} \approx 1000\text{--}2000\text{ ms}$)**: When transitioning from bright back to dark ($T$ increases), the black offset must rise very slowly and imperceptibly, avoiding "black breathing" or pumping artifacts.

#### Slew-Limited Exponential Filter:
Given monotonic elapsed time $\Delta t$:
$$\tau = \begin{cases} \tau_{\text{attack}} & \text{if } T < M_{\text{prev}} \\ \tau_{\text{release}} & \text{if } T \ge M_{\text{prev}} \end{cases}$$
$$\alpha = 1.0 - \exp\left( -\frac{\Delta t}{\tau} \right)$$
$$M_{\text{tentative}} = M_{\text{prev}} + \alpha \cdot (T - M_{\text{prev}})$$
$$M(t) = M_{\text{prev}} + \text{clamp}\left( M_{\text{tentative}} - M_{\text{prev}}, -\text{slew} \cdot \Delta t, +\text{slew} \cdot \Delta t \right)$$

### 4.4 Multi-Output Frame-Accurate Synchronization

In a multi-projector system, if individual displays update their temporal modulation factor on different frames, viewers will see jarring inter-display flicker.
- **Rule**: All outputs in the appliance share the exact same temporal modulation factor $M(t)$ tied to the logical capture generation ID.
- **Static Page Handling**: On static web pages with no new damage captures, a bounded 20 ms settling timer continues driving the envelope follower until $M(t)$ settles, repainting the retained textures without creating dummy Chromium frames.

---

## 5. API & Configuration Schema Evolution

### 5.1 JSON Configuration (`projection.blackOffset`)

```json
{
  "projection": {
    "mode": "warp",
    "blackOffset": {
      "mode": "dynamic",
      "amount": 0.05,
      "dynamic": {
        "kneeThreshold": 0.15,
        "curve": "smooth"
      },
      "temporal": {
        "darkThreshold": 0.02,
        "brightThreshold": 0.20,
        "attackMs": 80,
        "releaseMs": 1500,
        "slewPerSecond": 0.2
      }
    }
  }
}
```

### 5.2 Live Telemetry (`GET /api/v1/projection/stats`)

The stats response exposes live controller health:

```json
"blackOffset": {
  "requestedMode": "dynamic",
  "effectiveMode": "dynamic",
  "maxOverlap": 2,
  "configuredAmount": 0.05,
  "temporalModulation": 0.842,
  "effectiveAmount": 0.0421,
  "sceneLuminance": 0.045,
  "measurementAgeMs": 16.6,
  "limitationReason": null
}
```

---

## 6. Implementation & Verification Plan

### 6.1 Implementation Packets

- **Packet B1 (Phase 1 Runtime & Schema — Complete)**:
  - Add `BlackOffsetConfig` with `"off"` and `"constant"` modes.
  - Implement $N_{\max}$ topology detection and forced `"off"` rule.
  - Wire into existing `Coverage::lift` and `pixel_transfer` routines.
  - Tests: Topology overlap count unit tests, forced `"off"` when $N_{\max} \le 1$, gamma scaling tests.
- **Packet B2 (Phase 2 Dynamic 2D LUT)**:
  - Implement 2D LUT generator ($256 \times 256$ Signal $\times$ Deficit).
  - Add Vulkan / GLES 2D texture binding and update warp fragment shader.
  - Tests: Bit-exact roll-off validation at $Y=0$ (matches constant) and $Y \ge Y_{\text{thresh}}$ (equals zero). Memory leak and performance regression tests.
- **Packet B3 (Phase 3 Temporal Controller)**:
  - Implement pre-warp canvas luminance reduction pass.
  - Implement asymmetric slew-limited envelope follower with monotonic time.
  - Wire logical generation synchronization across all output presenters.
  - Tests: Step response, scene cut stability, static page settling timer, zero-sample robustness.
- **Packet B4 (Web UI Integration & Manual Optical Validation)**:
  - Add Black Offset controls to the Displays settings page (Mode selector, Amount slider, Dynamic toggle).
  - Physical camera optical verification in dark room measuring luminance uniformity across seams.
