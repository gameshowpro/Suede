//! Desired state: the document clients write and Suede persists.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::arrangement::Arrangement;
use super::black_lift::BlackLift;
use super::geometry::{validate_slices, CanvasConfig, OutputGeometry, ProjectionMode};
use super::observed::{Mode, Output, Position};

/// Current persisted-document schema. Alpha upgrades require this exact
/// version; older files are not migrated.
pub const SCHEMA_VERSION: u32 = 2;

/// The complete desired-state document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct DesiredState {
    /// Document schema version, managed by Suede.
    pub schema_version: u32,
    /// Monotonic revision, incremented by Suede on every accepted write.
    pub revision: u64,
    /// Whether this document is persisted, or a working copy being tried out.
    ///
    /// On reads, Suede reports the truth: `true` for the saved document,
    /// `false` when a working copy is live. On writes, the *client* speaks:
    /// `committed: true` persists; anything else applies the document to the
    /// outputs — reconciled immediately, exactly as if saved — but leaves
    /// disk untouched, so a restart or `POST /config/revert` returns to the
    /// saved state. A UI can therefore push every edit as it happens and
    /// only set the flag when the operator presses Save.
    #[serde(default)]
    pub committed: bool,
    pub outputs: Vec<OutputConfig>,
    pub apps: Vec<AppConfig>,
    /// Which app is running. `null` runs nothing.
    ///
    /// Exactly one app is ever active, and it always spans every display —
    /// the appliance is a single canvas, not a window manager. Keeping the
    /// choice as one pointer makes switching atomic: activating B cannot
    /// leave A half-enabled the way per-app flags could.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_app: Option<String>,
    /// Named background definitions outputs can refer to.
    ///
    /// A multi-display installation usually wants one look across every
    /// alternative — repeating a wallpaper, scaling mode and color on each
    /// output — makes the common case the laborious one and guarantees the
    /// displays drift apart the first time somebody edits only three of four.
    pub backgrounds: Vec<BackgroundPreset>,
    /// Multi-projector features. Absent means no projection processing at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<ProjectionConfig>,
    pub settings: Settings,
}

/// Shared multi-projector content layout and retained Warp correction.
///
/// There is no overlap setting here, because the slice layout determines
/// overlap. Simple and Warp share the configured canvas and normalized slice
/// rectangles. Warp adds destination correction to that content selection.
/// Without an explicit canvas, integer output positions define the layout.
///
/// Sway never sees any of this. It is always handed a plain edge-to-edge
/// tiling; the active app renders into a headless canvas the size of the
/// layout's bounding box; and the slicer cuts that canvas into each
/// projector's configured rectangle, duplicating the intersections and
/// fading them from both sides when `blend` is on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ProjectionConfig {
    /// Which geometry pipeline is requested. Warp settings remain retained
    /// while simple mode is active.
    pub mode: ProjectionMode,
    /// Shared canvas dimensions for Simple, Warp, and capability fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canvas: Option<CanvasConfig>,
    /// Master switch for seam ramps. `false` retains required slicing and warp
    /// processing but disables the transfer fades.
    pub blend: bool,
    /// The projectors' transfer gamma, shaping every ramp's fall-off.
    ///
    /// A ramp that is linear in signal is not linear in light: the display
    /// raises the signal to `gamma`. Each ramp is therefore pre-shaped as
    /// `ramp^(1/gamma)` so that the *luminance* of the two overlapping
    /// projectors sums to a constant across the seam. One value for the whole
    /// installation — these are near-universally identical projectors; per-output
    /// overrides can be added later if mixed models ever matter.
    pub gamma: f64,
    /// Black-level compensation, `0.0` (off) to `0.5`.
    ///
    /// Projector black is not zero light, so seams glow in dark scenes: they
    /// receive two projectors' worth of leaked black. The fix cannot darken
    /// the seam, so it lifts the signal everywhere *else* to match —
    /// `out = lift + (1 − lift)·in`. On a black scene, raise this until the
    /// un-doubled regions match the seams.
    pub black_lift: BlackLift,
    /// Show a built-in test pattern instead of the content. `null` is off.
    ///
    /// Patterns draw in *global* coordinates, so features continue exactly
    /// across a seam — two aligned projectors superimpose them perfectly.
    /// They are the bench-verification and field-alignment tool: the blend
    /// ramps and black lift still apply on top, exactly as they would to
    /// real content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_pattern: Option<TestPattern>,
    /// Let each output take frames at its own pace.
    ///
    /// Off (the default), the slicer commits a frame to every output together
    /// and does not commit the next until all of them have taken it, so the
    /// same frame is on every display at once. On, each output is handed the
    /// newest frame the moment it is ready for one: displays at different
    /// refresh rates each run at their own, and the wall gives up being in
    /// step. Only for installations that cannot share a rate.
    pub free_run: bool,
    /// Which pipeline the slicer composites with; see [`Renderer`].
    pub renderer: Renderer,
    /// The grid arrangement last applied, as a record of what was asked for.
    ///
    /// Explicit geometry stays canonical: this holds the resolved rows,
    /// columns, overlaps and content scale so a slider client can pick up
    /// where the last one left off, and nothing else reads it. A later
    /// manual geometry edit leaves it in place — see
    /// [`crate::model::arrangement::in_effect`], which reports whether it
    /// still describes the document's slices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrangement: Option<Arrangement>,
    /// Ephemeral settings applied to the working copy like any other field,
    /// but never persisted. See [`TemporarySettings`].
    #[serde(default)]
    pub temporary: TemporarySettings,
}

impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            mode: ProjectionMode::Simple,
            canvas: None,
            blend: true,
            gamma: 2.2,
            black_lift: BlackLift::default(),
            test_pattern: None,
            free_run: false,
            renderer: Renderer::Auto,
            arrangement: None,
            temporary: TemporarySettings::default(),
        }
    }
}

/// Ephemeral, never-persisted settings: applied to the working copy exactly
/// like any other field, but reset to default wherever a document is about
/// to reach disk.
///
/// That reset is structural, not a client convention: `commit_if` and
/// `commit_literal_if` (`src/api/mod.rs`) reset this struct to its default,
/// unconditionally, right before their `stage_replace_if` closures return —
/// so it is categorically impossible for any caller, this UI or any future
/// or API-direct one, to persist a non-default value here. The other reset
/// points fall out of that same guarantee for free: cancel
/// (`clear_preview_if` discards the whole preview, the only place this
/// could be non-default), daemon startup/disk load (the persisted document
/// can never contain a non-default value, per the commit-time reset), and
/// an explicit toggle back off (an ordinary write like any other).
///
/// This is the intended home for future ephemeral, render-affecting
/// toggles — debug overlays and the like — that only make sense on the live
/// working copy. `test_pattern` above predates this struct and stays where
/// it is (moving it would be a breaking rename of an established field); it
/// is only *conventionally* non-persistent today, relying on a well-behaved
/// client to strip it before every commit. New fields of this kind belong
/// here instead, where the guarantee is enforced rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct TemporarySettings {
    /// Overlay two-pixel orange/blue seam-boundary marker lines on every
    /// projector output, to aid warp lineup. Off by default, and meaningful
    /// only while edge blending (`blend`) is on.
    #[serde(default)]
    pub highlight_overlaps: bool,
}

/// Which pipeline the slicer uses to composite the canvas onto each output.
///
/// `Auto` (the default) prefers the GPU path — the compositor blits the
/// canvas into a Vulkan image exported as a dmabuf, a fragment shader blends
/// it straight into each output's own dmabuf, and no pixel ever crosses to
/// system memory — falling back to the CPU path (shared-memory screencopy,
/// blended on the CPU) whenever the compositor does not offer dmabuf capture
/// or Vulkan fails to initialize. `Cpu` forces the fallback path even on
/// hardware that could do better. `Gpu` forces the GPU path and is a startup
/// error if the machine cannot actually provide it — the slicer exits and
/// the daemon respawns it on its next reconcile, rather than silently
/// running the slower path — for a rig where that would go unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    #[default]
    Auto,
    Cpu,
    Gpu,
}

/// A built-in projection test pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TestPattern {
    /// Colored 100 px tiles with crosses, global pixel coordinates, and the
    /// output name — for geometry, focus, and seam alignment.
    Grid,
    /// Full white: shows the blend ramps in isolation and exposes brightness
    /// mismatch between projectors.
    White,
    /// Full black: for tuning `blackLift` — raise the lift until the
    /// un-doubled regions match the glowing seams.
    Black,
    /// Gamma measurement: candidate patches beside a stripe field that
    /// averages to half light. The patch that matches from a distance names
    /// the projector's gamma; the configured value is marked.
    Gamma,
    /// The connector's name, as large as the output will carry, on a color
    /// unique to that name.
    ///
    /// For deciding which cable to move. The grid carries the name too, but
    /// in 5-pixel text in the corner of every tile: legible in a photograph,
    /// useless from the back of a room with a cable in your hand. Reading it
    /// off the wall is the whole job here, so everything else gets out of
    /// the way.
    Identify,
    /// An animated frame counter for measuring output-to-output presentation
    /// sync with a high-speed camera.
    ///
    /// Unlike every other pattern here, this one is drawn *per frame* by the
    /// slicer, through the same present path content uses — so a fast-shutter
    /// photograph of two projectors showing different numbers is a direct
    /// measurement of Suede's own presentation path, with no browser in it.
    /// Needs `allowOverlaps = true` (the slicer): the tiled path's static
    /// overlays cannot animate and show a placeholder instead.
    ///
    /// Each output carries two digits of the counter, a 16-bit binary strip
    /// of it beside them, four large cells along the bottom edge holding its
    /// low four bits, the output's name top left, and the snapshot id with a
    /// UTC `HH:MM:SS.mmm` clock bottom left. The digits and the strip are for
    /// reading off a still; the four big cells and the clock are for a video
    /// clip a script measures frame by frame, and for tying that clip back to
    /// the stats log and the journal.
    Sync,
    /// 10% grid lines and center alignment circle on dark gray for warp geometry.
    #[serde(rename = "warp-alignment")]
    WarpAlignment,
}

impl DesiredState {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        }
    }

    /// Semantic validation beyond what serde enforces. Returns all problems found.
    ///
    /// `allow_overlaps` is the bootstrap flag of the same name: it says which
    /// of the two display paths this machine runs, and so whether a layout
    /// whose rectangles intersect is a legitimate projector overlap or a
    /// mistake. It is not part of the document, because it is a fact about
    /// how the compositor was started rather than something a client may
    /// choose — see [`crate::config::BootstrapConfig::allow_overlaps`].
    pub fn validate(&self, allow_overlaps: bool) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        self.validate_outputs_basic(&mut errors);
        self.validate_layout_topology(allow_overlaps, &mut errors);
        self.validate_backgrounds(&mut errors);
        self.validate_projection(allow_overlaps, &mut errors);
        self.validate_apps(&mut errors);
        self.validate_settings(&mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Per-output match, mode, scale and background checks that do not
    /// depend on any other output or on the layout as a whole.
    fn validate_outputs_basic(&self, errors: &mut Vec<String>) {
        let mut seen_outputs = std::collections::HashSet::new();
        for (index, output) in self.outputs.iter().enumerate() {
            let prefix = format!("outputs[{index}]");
            if output.r#match.is_empty() {
                errors.push(format!(
                    "{prefix}.match must set at least one of name, make, model, serial"
                ));
            }
            if !seen_outputs.insert(output.r#match.key()) {
                errors.push(format!(
                    "{prefix}.match duplicates an earlier entry ({})",
                    output.r#match.key()
                ));
            }
            if let Some(mode) = &output.mode {
                if mode.width <= 0 || mode.height <= 0 {
                    errors.push(format!("{prefix}.mode dimensions must be positive"));
                }
                if mode.refresh_hz <= 0.0 {
                    errors.push(format!("{prefix}.mode.refreshHz must be positive"));
                }
            }
            if let Some(scale) = output.scale {
                if !(scale.is_finite() && scale > 0.0) {
                    errors.push(format!("{prefix}.scale must be a positive number"));
                }
            }
            match &output.background {
                Some(BackgroundRef::Inline(background)) => {
                    errors.extend(background.problems(&format!("{prefix}.background")));
                }
                // Caught here rather than at reconcile time: a typo in a preset
                // name is a mistake in the write, and the writer is the only
                // one who can still fix it cheaply.
                Some(BackgroundRef::Preset(id))
                    if !self.backgrounds.iter().any(|preset| &preset.id == id) =>
                {
                    errors.push(format!(
                        "{prefix}.background {id:?} is not a defined background preset"
                    ));
                }
                Some(BackgroundRef::Preset(_)) => {}
                None => {}
            }
        }
    }

    /// The layout must be one contiguous surface: every output chained back
    /// to the first through any run of overlaps or shared edges. A gap in
    /// the chain means part of the canvas maps to no projector at all —
    /// content silently lost — and there is no use case for it. Only
    /// entries whose rectangle the document actually pins down (enabled,
    /// with both mode and position) can be checked; the rest take their
    /// geometry from whatever is observed at reconcile time.
    ///
    /// Flood fill over pairwise touch tests. At the design maximum of four
    /// outputs that is at most a dozen integer comparisons —
    /// asymptotically cleverer schemes (sweep lines) only pay for
    /// themselves at hundreds of rectangles.
    fn validate_layout_topology(&self, allow_overlaps: bool, errors: &mut Vec<String>) {
        let rects: Vec<(String, i64, i64, i64, i64)> = self
            .outputs
            .iter()
            .filter(|output| output.enable)
            // A shared crop supersedes an output's legacy integer position,
            // so validating both would reject a good crop over stale
            // compositor coordinates — but that only excuses the outputs
            // that actually carry a slice rectangle. A canvas being
            // configured elsewhere in the document must not exempt an
            // output with no geometry of its own from this check.
            .filter(|output| output.geometry.is_none())
            .filter_map(|output| {
                // The adopted mode counts too: once a value has been pinned,
                // it is as settled a fact as one the operator typed in, and
                // a layout that only validates the operator's own half would
                // wrongly exempt an output the moment it gets adopted.
                let mode = output.effective_mode()?;
                let position = output.position?;
                Some((
                    output.r#match.key(),
                    position.x as i64,
                    position.y as i64,
                    mode.width as i64,
                    mode.height as i64,
                ))
            })
            .collect();
        if rects.len() > 1 && !allow_overlaps {
            // Sway is handed this layout verbatim on a tiling appliance, and
            // its single global coordinate space gives every output the same
            // pixels in a shared region — so an overlap is not a projector
            // overlap here, it is a layout that cannot be rendered as drawn.
            // Rejected rather than clamped, for the same reason a gap is: the
            // API never alters a document it was given, it says what is wrong
            // with it and leaves the fix to whoever wrote it.
            for i in 0..rects.len() {
                for j in (i + 1)..rects.len() {
                    let (a, b) = (&rects[i], &rects[j]);
                    let ox = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
                    let oy = (a.2 + a.4).min(b.2 + b.4) - a.2.max(b.2);
                    if ox > 0 && oy > 0 {
                        errors.push(format!(
                            "outputs {} and {} overlap by {ox}x{oy} pixels, and this \
                             appliance tiles: set allow_overlaps = true in suede.toml \
                             to project overlapping layouts through the slicer, or \
                             move them apart",
                            a.0, b.0
                        ));
                    }
                }
            }
        }
        if rects.len() > 1 {
            // Chained = closed rectangles meeting in more than a single
            // point: overlap, or a shared edge segment. A corner-to-corner
            // touch is not a chain — no pixel row or column is continuous
            // across it.
            let chained = |a: &(String, i64, i64, i64, i64), b: &(String, i64, i64, i64, i64)| {
                let ox = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
                let oy = (a.2 + a.4).min(b.2 + b.4) - a.2.max(b.2);
                ox >= 0 && oy >= 0 && (ox > 0 || oy > 0)
            };
            let mut reached = vec![false; rects.len()];
            reached[0] = true;
            let mut frontier = vec![0usize];
            while let Some(current) = frontier.pop() {
                for (index, rect) in rects.iter().enumerate() {
                    if !reached[index] && chained(&rects[current], rect) {
                        reached[index] = true;
                        frontier.push(index);
                    }
                }
            }
            let stranded: Vec<&str> = rects
                .iter()
                .zip(&reached)
                .filter(|(_, reached)| !**reached)
                .map(|(rect, _)| rect.0.as_str())
                .collect();
            if !stranded.is_empty() {
                errors.push(format!(
                    "layout is not contiguous: {} cannot be chained back to {} \
                     through overlapping or edge-sharing outputs",
                    stranded.join(", "),
                    rects[0].0
                ));
            }
        }
    }

    fn validate_backgrounds(&self, errors: &mut Vec<String>) {
        let mut seen_backgrounds = std::collections::HashSet::new();
        for (index, preset) in self.backgrounds.iter().enumerate() {
            let prefix = format!("backgrounds[{index}]");
            if preset.id.trim().is_empty() {
                errors.push(format!("{prefix}.id must not be empty"));
            }
            if !seen_backgrounds.insert(preset.id.clone()) {
                errors.push(format!("{prefix}.id {:?} is not unique", preset.id));
            }
            errors.extend(preset.background.problems(&prefix));
        }
    }

    /// Validate one output's retained geometry exactly once: against the
    /// raster it will actually be sampled onto when a canvas and an
    /// effective mode are both known, otherwise against the canvas (or
    /// canvas-less numeric bounds) alone. Callers must not also call
    /// [`OutputGeometry::validate_for_output`] or
    /// [`OutputGeometry::validate_numbers`] themselves for the same output —
    /// that was the duplicate-error bug this exists to close.
    fn validate_output_geometry_once(
        geometry: &OutputGeometry,
        canvas: Option<&CanvasConfig>,
        mode: Option<Mode>,
        prefix: &str,
        errors: &mut Vec<String>,
    ) {
        let result = match (canvas, mode) {
            (Some(canvas), Some(mode)) if mode.width > 0 && mode.height > 0 => {
                geometry.validate_for_output(canvas, mode.width as u32, mode.height as u32)
            }
            _ => geometry.validate_numbers(canvas),
        };
        if let Err(error) = result {
            errors.push(format!("{prefix}.geometry {error}"));
        }
    }

    /// Projection configuration: canvas, gamma, black lift, and (for a
    /// shared canvas, in either pipeline) each participating output's
    /// geometry and presentation dimensions.
    fn validate_projection(&self, allow_overlaps: bool, errors: &mut Vec<String>) {
        let Some(projection) = &self.projection else {
            // Retained warp entries remain part of the document even when no
            // projection pipeline is requested, so malformed numbers cannot
            // hide behind the absent pipeline.
            for (index, output) in self.outputs.iter().enumerate() {
                if let Some(geometry) = &output.geometry {
                    Self::validate_output_geometry_once(
                        geometry,
                        None,
                        None,
                        &format!("outputs[{index}]"),
                        errors,
                    );
                }
            }
            return;
        };

        if let Some(canvas) = projection.canvas.as_ref() {
            if let Err(error) = canvas.dimensions() {
                errors.push(format!("projection.canvas {error}"));
            }
        }
        // 1.0 disables the correction; beyond 4.0 is no known display and
        // almost certainly a typo'd measurement (22 for 2.2).
        if !(projection.gamma.is_finite() && (1.0..=4.0).contains(&projection.gamma)) {
            errors.push(format!(
                "projection.gamma must be between 1.0 and 4.0, not {}",
                projection.gamma
            ));
        }
        // Above 0.5 the "compensation" is brighter than mid-gray, which
        // is no black level anyone measured.
        if let Err(error) = projection.black_lift.validate() {
            errors.push(format!("projection.{error}"));
        }
        // A record of intent, but a nonsensical one would be handed straight
        // back to a client as the starting point for its next solve.
        if let Some(arrangement) = &projection.arrangement {
            if arrangement.rows == 0 || arrangement.columns == 0 {
                errors.push("projection.arrangement rows and columns must be at least 1".into());
            }
            for (label, overlap) in [
                ("overlapX", arrangement.overlap_x),
                ("overlapY", arrangement.overlap_y),
            ] {
                if !(0.0..1.0).contains(&overlap) {
                    errors.push(format!(
                        "projection.arrangement.{label} must be at least 0 and less than 1, not {overlap}"
                    ));
                }
            }
            if !(arrangement.content_scale.is_finite() && arrangement.content_scale > 0.0) {
                errors.push(format!(
                    "projection.arrangement.contentScale must be a positive number, not {}",
                    arrangement.content_scale
                ));
            }
        }
        // A per-output nudge applied only by the grid arrangement solve,
        // validated beside the arrangement record it works alongside. `16`
        // matches the `±16` canvas-span bound `geometry.slice` is held to
        // (`MAX_SLICE_SPAN` in `geometry.rs`) — an offset need never be
        // larger than a slice rectangle is allowed to stray from the canvas.
        for (index, output) in self.outputs.iter().enumerate() {
            let Some(offset) = output.arrange_offset else {
                continue;
            };
            for (axis, value) in [("x", offset.x), ("y", offset.y)] {
                if !(value.is_finite() && value.abs() <= 16.0) {
                    errors.push(format!(
                        "outputs[{index}].arrangeOffset.{axis} must be finite and at most 16 in \
                         magnitude, not {value}"
                    ));
                }
            }
        }

        // A shared canvas has one complete configured roster in either
        // pipeline, including disconnected enabled outputs.
        let shared = projection.canvas.is_some() || projection.mode == ProjectionMode::Warp;
        // One fact about the whole document, not one per output: emitted
        // here instead of inside the per-output loop below, which used to
        // repeat it once for every enabled output.
        if shared && projection.canvas.is_none() {
            errors.push("projection.canvas is required in warp mode".into());
        }
        let mut warp_slices = Vec::new();
        let enabled_count = self.outputs.iter().filter(|output| output.enable).count();
        for (index, output) in self.outputs.iter().enumerate() {
            let prefix = format!("outputs[{index}]");
            // Exactly one geometry validation per output, regardless of
            // whether it also takes part in the shared-canvas checks below.
            if let Some(geometry) = &output.geometry {
                Self::validate_output_geometry_once(
                    geometry,
                    projection.canvas.as_ref(),
                    output.effective_mode(),
                    &prefix,
                    errors,
                );
            }
            if !(shared && output.enable) {
                continue;
            }
            if projection.canvas.is_none() {
                // Already reported once above, for the document as a whole.
                continue;
            }
            let Some(mode) = output.effective_mode() else {
                errors.push(format!("{prefix}.mode is required for a shared canvas"));
                continue;
            };
            if projection.mode == ProjectionMode::Warp
                && output.effective_scale().is_some_and(|scale| scale != 1.0)
            {
                errors.push(format!(
                    "{prefix}.scale must be 1.0 in warp mode until raster scaling is supported"
                ));
            }
            if projection.mode == ProjectionMode::Warp
                && output
                    .effective_transform()
                    .is_some_and(|transform| transform != Transform::Normal)
            {
                errors.push(format!(
                    "{prefix}.transform must be normal in warp mode until transformed rasters are supported"
                ));
            }
            if mode.width <= 0 || mode.height <= 0 {
                errors.push(format!("{prefix}.mode dimensions must be positive"));
                continue;
            }
            if mode.width > super::limits::MAX_DIMENSION as i32
                || mode.height > super::limits::MAX_DIMENSION as i32
            {
                errors.push(format!(
                    "{prefix}.mode dimensions must be at most 32768 pixels per axis"
                ));
            }
            if (mode.width as u64) * (mode.height as u64) > super::limits::MAX_OUTPUT_PIXELS {
                errors.push(format!("{prefix}.mode allocation exceeds the 32MP limit"));
            }
            if let Err(error) =
                output.presentation_dimensions(mode, projection.mode == ProjectionMode::Warp)
            {
                errors.push(format!("{prefix} {error}"));
            }
            match &output.geometry {
                // Already validated once above (against this same canvas
                // and mode); only the slice needs collecting here.
                Some(geometry) => warp_slices.push(geometry.slice),
                None => errors.push(format!("{prefix}.geometry is required for a shared canvas")),
            }
        }
        if shared {
            if !allow_overlaps {
                errors.push("shared canvas rendering requires allow_overlaps = true".into());
            }
            if enabled_count == 0 {
                errors.push("shared canvas rendering requires at least one enabled output".into());
            }
            if let Some(canvas) = projection.canvas.as_ref() {
                if warp_slices.len() == enabled_count && !warp_slices.is_empty() {
                    if let Err(error) = validate_slices(canvas, &warp_slices) {
                        errors.push(format!("projection slices {error}"));
                    }
                }
            }
        }
    }

    fn validate_apps(&self, errors: &mut Vec<String>) {
        if let Some(active) = &self.active_app {
            if !self.apps.iter().any(|app| &app.id == active) {
                errors.push(format!("activeApp {active:?} does not name a listed app"));
            }
        }

        let mut seen_apps = std::collections::HashSet::new();
        for (index, app) in self.apps.iter().enumerate() {
            let prefix = format!("apps[{index}]");
            if app.id.trim().is_empty() {
                errors.push(format!("{prefix}.id must not be empty"));
            } else if !app
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                errors.push(format!(
                    "{prefix}.id {:?} may only contain letters, digits, '-', '_' and '.'",
                    app.id
                ));
            }
            // The id becomes a directory name under the state directory.
            if app.id.contains("..") || app.id == "." {
                errors.push(format!(
                    "{prefix}.id {:?} must not be a relative path fragment",
                    app.id
                ));
            }
            if !seen_apps.insert(app.id.clone()) {
                errors.push(format!("{prefix}.id {:?} is not unique", app.id));
            }
            match &app.launcher {
                Launcher::ChromiumKiosk { uri, .. } | Launcher::FirefoxKiosk { uri, .. } => {
                    if uri.trim().is_empty() {
                        errors.push(format!("{prefix}.launcher.uri must not be empty"));
                    }
                }
                Launcher::Exec { command, .. } => {
                    if command.trim().is_empty() {
                        errors.push(format!("{prefix}.launcher.command must not be empty"));
                    }
                }
            }
            if app.restart.delay_ms > app.restart.max_delay_ms {
                errors.push(format!(
                    "{prefix}.restart.delayMs must not exceed maxDelayMs"
                ));
            }
            if let Some(audio) = &app.audio {
                // The scale runs from silence to unity: -100 dB stands in
                // for minus infinity, and there is nothing above 0 dB that a
                // digital sink can do except clip.
                if !(audio.gain_db.is_finite()
                    && (crate::audio::GAIN_FLOOR_DB..=crate::audio::GAIN_CEILING_DB)
                        .contains(&audio.gain_db))
                {
                    errors.push(format!(
                        "{prefix}.audio.gainDb must be between {} and {}, not {}",
                        crate::audio::GAIN_FLOOR_DB,
                        crate::audio::GAIN_CEILING_DB,
                        audio.gain_db
                    ));
                }
            }
            if let Some(heartbeat) = &app.heartbeat {
                if heartbeat.enabled && heartbeat.timeout_seconds == 0 {
                    errors.push(format!(
                        "{prefix}.heartbeat.timeoutSeconds must be greater than zero"
                    ));
                }
            }
            for key in app.env.keys() {
                if key.is_empty() || key.contains('=') || key.contains('\0') {
                    errors.push(format!("{prefix}.env has an invalid variable name {key:?}"));
                }
            }
            if let Some(readiness) = &app.readiness {
                // Caught here rather than at launch, where a bad URL would
                // present as an application that simply never starts.
                if let Err(error) = crate::probe::parse_url(&readiness.url) {
                    errors.push(format!("{prefix}.readiness.url {error}"));
                }
                if readiness.interval_seconds == 0 {
                    errors.push(format!(
                        "{prefix}.readiness.intervalSeconds must be greater than zero"
                    ));
                }
                if readiness.timeout_seconds == 0 {
                    errors.push(format!(
                        "{prefix}.readiness.timeoutSeconds must be greater than zero"
                    ));
                }
            }
        }
    }

    fn validate_settings(&self, errors: &mut Vec<String>) {
        if self.settings.output_poll_interval_seconds == 0 {
            errors.push("settings.outputPollIntervalSeconds must be greater than zero".into());
        }
    }

    pub fn app(&self, id: &str) -> Option<&AppConfig> {
        self.apps.iter().find(|a| a.id == id)
    }
}

/// Rule selecting which physical output a config entry applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputMatch {
    /// Connector name, e.g. `HDMI-A-1`. The default and most direct way to match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// EDID manufacturer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    /// EDID model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// EDID serial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl OutputMatch {
    pub fn by_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.make.is_none() && self.model.is_none() && self.serial.is_none()
    }

    /// Whether this rule selects `output`. Every specified field must match.
    pub fn matches(&self, output: &Output) -> bool {
        if self.is_empty() {
            return false;
        }
        let eq = |rule: &Option<String>, actual: &Option<String>| match rule {
            None => true,
            Some(want) => actual.as_deref().is_some_and(|have| have == want),
        };
        self.name.as_ref().is_none_or(|want| output.name == *want)
            && eq(&self.make, &output.make)
            && eq(&self.model, &output.model)
            && eq(&self.serial, &output.serial)
    }

    /// Stable key used to address this entry in the API and to detect duplicates.
    pub fn key(&self) -> String {
        if let Some(name) = &self.name {
            if self.make.is_none() && self.model.is_none() && self.serial.is_none() {
                return name.clone();
            }
        }
        let part = |value: &Option<String>| value.clone().unwrap_or_default();
        format!(
            "edid:{}:{}:{}:{}",
            part(&self.name),
            part(&self.make),
            part(&self.model),
            part(&self.serial)
        )
    }

    /// Inverse of [`OutputMatch::key`].
    pub fn parse_key(key: &str) -> Self {
        if let Some(rest) = key.strip_prefix("edid:") {
            let parts: Vec<&str> = rest.splitn(4, ':').collect();
            let get = |i: usize| {
                parts
                    .get(i)
                    .filter(|v| !v.is_empty())
                    .map(|v| (*v).to_string())
            };
            Self {
                name: get(0),
                make: get(1),
                model: get(2),
                serial: get(3),
            }
        } else {
            Self::by_name(key)
        }
    }
}

/// Output rotation and flipping, as accepted by `sway-output(5)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
pub enum Transform {
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    #[serde(rename = "flipped")]
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

impl Transform {
    pub fn as_sway(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Rotate90 => "90",
            Self::Rotate180 => "180",
            Self::Rotate270 => "270",
            Self::Flipped => "flipped",
            Self::Flipped90 => "flipped-90",
            Self::Flipped180 => "flipped-180",
            Self::Flipped270 => "flipped-270",
        }
    }

    /// Inverse of [`Transform::as_sway`], parsing what `get_outputs` reports
    /// back. Needed to compare a freshly observed transform against a
    /// previous one when deciding whether a value has settled — see
    /// [`AdoptedOutput`].
    pub fn from_sway(value: &str) -> Option<Self> {
        Some(match value {
            "normal" => Self::Normal,
            "90" => Self::Rotate90,
            "180" => Self::Rotate180,
            "270" => Self::Rotate270,
            "flipped" => Self::Flipped,
            "flipped-90" => Self::Flipped90,
            "flipped-180" => Self::Flipped180,
            "flipped-270" => Self::Flipped270,
            _ => return None,
        })
    }
}

/// What Suede observed and pinned because the configuration named none.
///
/// Kept apart from the operator's own fields, not merged into them, because
/// the two must stay distinguishable forever: an operator who asked for
/// 1920x1200 and cannot have it deserves a divergence that persists until a
/// human decides, while a value that is merely what happened to be plugged in
/// last week should be replaced without ceremony when the display changes.
/// Merged into one field, nothing could tell those two cases apart.
///
/// Deliberately absent: **position**. In projection mode the configured
/// position is a canvas coordinate where the beams overlap, while sway is
/// handed a plain edge-to-edge tiling, so observed position is not desired
/// position by design — on the four-projector bench the configuration holds
/// a 2x2 grid while sway reports a single row, and adopting observed
/// positions would flatten the layout and destroy the blend. Do not add it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdoptedOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    /// The display these were taken from, so that swapping the display on a
    /// connector re-adopts rather than pinning the old one's values forever.
    /// `None` when the display reported no EDID identity at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayIdentity>,
    /// Unix seconds, so an operator can see how old a pin is.
    #[serde(default)]
    pub captured_at: u64,
}

/// Enough of a display's EDID to tell "the same display" from "a different
/// one on the same connector".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisplayIdentity {
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

impl DisplayIdentity {
    /// Build from what was observed. `None` when `output` carries no EDID
    /// identity at all, so "no identity either time" never reads as a
    /// display change.
    pub fn of(output: &Output) -> Option<Self> {
        if output.make.is_none() && output.model.is_none() && output.serial.is_none() {
            return None;
        }
        Some(Self {
            make: output.make.clone(),
            model: output.model.clone(),
            serial: output.serial.clone(),
        })
    }
}

/// Desired configuration for one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputConfig {
    /// Which physical output this entry applies to.
    pub r#match: OutputMatch,
    /// Whether the output should be enabled. `false` actively disables it.
    #[serde(default = "default_true")]
    pub enable: bool,
    /// Mode to apply. When absent, Sway's preferred mode is left in place —
    /// and, once settled, pinned into `adopted.mode`. Reading this field
    /// directly sees only the operator's own choice; almost every reader
    /// wants [`OutputConfig::effective_mode`] instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Position in the global layout. Suede performs no layout arithmetic.
    /// Never adopted — see [`AdoptedOutput`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    /// Shared normalized content slice plus retained Warp correction and
    /// independent physical light footprint. Simple uses the same slice.
    /// Kept independently from `position` and from the requested pipeline so
    /// switching to simple mode does not discard calibrated warp settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<OutputGeometry>,
    /// When absent, left as Sway's own and, once settled, pinned into
    /// `adopted.scale`. Prefer [`OutputConfig::effective_scale`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// When absent, left as Sway's own and, once settled, pinned into
    /// `adopted.transform`. Prefer [`OutputConfig::effective_transform`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    #[serde(default)]
    pub adaptive_sync: bool,
    /// Applied only when the detected Sway version supports it (≥ 1.10).
    #[serde(default)]
    pub allow_tearing: bool,
    /// Maximum milliseconds allowed to render a frame; `null` means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_render_time_ms: Option<u32>,
    /// What this output shows when no window covers it: a preset name, or the
    /// properties spelled out. See [`BackgroundRef`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundRef>,
    /// What Suede observed and pinned because the configuration named none.
    ///
    /// Kept apart from the operator's own fields, not merged into them, because
    /// the two must stay distinguishable forever: an operator who asked for
    /// 1920x1200 and cannot have it deserves a divergence that persists until a
    /// human decides, while a value that is merely what happened to be plugged in
    /// last week should be replaced without ceremony when the display changes.
    /// Merged into one field, nothing could tell those two cases apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted: Option<AdoptedOutput>,
    /// A per-output nudge applied only by the grid arrangement solve; see
    /// [`ArrangeOffset`]. Absent means `{0, 0}` — the common case, and the
    /// only value the reference UI ever writes (it locks the field at zero
    /// but preserves whatever a third party set here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrange_offset: Option<ArrangeOffset>,
}

/// A per-output nudge the grid arrangement solve adds to this output's
/// computed slice, in normalized canvas units — the same space as
/// [`super::geometry::OutputGeometry::slice`].
///
/// Applied only by [`crate::model::arrangement::solve`], after placement:
/// coverage (`unusedCanvas`) and the strict full-coverage gate are computed
/// on the *pre-offset* rectangles, since an offset is a deliberate shift
/// (mechanical alignment, for instance) that may uncover canvas pixels on
/// purpose rather than a coverage failure. [`crate::model::arrangement::in_effect`]
/// applies the document's current offset before comparing, so editing the
/// offset after an arrangement has been applied turns `inEffect` false, like
/// any other manual geometry edit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArrangeOffset {
    pub x: f64,
    pub y: f64,
}

/// How a wallpaper is scaled onto an output, matching `sway-output(5)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundMode {
    /// Scale to cover, cropping the overflow. The usual choice for signage.
    #[default]
    Fill,
    /// Scale to fit entirely, letterboxing the remainder.
    Fit,
    /// Scale to the output exactly, ignoring aspect ratio.
    Stretch,
    /// Original size, centered.
    Center,
    /// Original size, repeated.
    Tile,
}

impl BackgroundMode {
    pub fn as_sway(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Fit => "fit",
            Self::Stretch => "stretch",
            Self::Center => "center",
            Self::Tile => "tile",
        }
    }
}

/// Color shown where no wallpaper reaches.
///
/// Black rather than "nothing": an unpainted output is whatever the compositor
/// last left there, which on a display appliance is usually a stale frame of the
/// previous app. Something deliberate is always better than something leftover.
pub const DEFAULT_BACKGROUND_COLOR: &str = "#000000";

/// What an output shows behind, or instead of, any window.
///
/// An appliance with a blank display looks broken even when it is merely
/// between launches, so a background gives it something deliberate to show
/// while a browser restarts or before the first app starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Background {
    /// Id of an uploaded wallpaper. Absent means use `color` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallpaper: Option<String>,
    /// `#rrggbb`, shown where the wallpaper does not reach, or on its own.
    /// Absent means [`DEFAULT_BACKGROUND_COLOR`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub mode: BackgroundMode,
}

impl Background {
    /// Everything wrong with this background, prefixed for the caller's path.
    ///
    /// Shared so an inline background and a preset are held to identical
    /// rules — they end up in the same swaybg command either way.
    pub fn problems(&self, prefix: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(color) = &self.color {
            let digits = color.trim_start_matches('#');
            let valid =
                matches!(digits.len(), 6 | 8) && digits.chars().all(|c| c.is_ascii_hexdigit());
            if !valid {
                errors.push(format!(
                    "{prefix}.color {color:?} must be #rrggbb or #rrggbbaa"
                ));
            }
        }
        if let Some(id) = &self.wallpaper {
            if id.is_empty() || id.contains("..") || id.contains('/') {
                errors.push(format!(
                    "{prefix}.wallpaper {id:?} is not a valid wallpaper id"
                ));
            }
        }
        errors
    }

    /// Sway wants `#rrggbb` without the hash. Never empty: an unset color
    /// falls back to black rather than leaving swaybg to invent one.
    pub fn sway_color(&self) -> String {
        self.color
            .as_deref()
            .unwrap_or(DEFAULT_BACKGROUND_COLOR)
            .trim_start_matches('#')
            .to_string()
    }
}

/// A named background, defined once and used by any number of outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundPreset {
    /// Client-chosen name, referenced from an output's `background`.
    pub id: String,
    #[serde(flatten)]
    pub background: Background,
}

/// What an output's `background` may be.
///
/// A bare string names a preset; an object spells the properties out. Both are
/// accepted because they serve different callers: the UI wants one dropdown
/// across every output, while a script driving the API directly should not
/// have to create a preset to paint one output.
///
/// ```json
/// "background": "lobby"
/// "background": { "wallpaper": "teal", "mode": "fill", "color": "#101820" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum BackgroundRef {
    /// Id of an entry in [`DesiredState::backgrounds`].
    Preset(String),
    /// Properties given directly.
    Inline(Background),
}

impl BackgroundRef {
    /// The preset this refers to, if it is a reference rather than inline.
    pub fn preset_id(&self) -> Option<&str> {
        match self {
            Self::Preset(id) => Some(id),
            Self::Inline(_) => None,
        }
    }

    /// Resolve to concrete properties against `presets`.
    ///
    /// `None` means the reference names a preset that does not exist — the
    /// caller raises a divergence rather than silently painting the display.
    pub fn resolve<'a>(&'a self, presets: &'a [BackgroundPreset]) -> Option<&'a Background> {
        match self {
            Self::Inline(background) => Some(background),
            Self::Preset(id) => presets
                .iter()
                .find(|preset| &preset.id == id)
                .map(|preset| &preset.background),
        }
    }
}

/// Wait for a URL to answer before launching an application.
///
/// A kiosk browser started before the service it points at is serving shows an
/// error page and stays there, since nothing reloads it. Gating the launch on
/// the service answering removes that race entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadinessConfig {
    /// URL to poll. Only `http://` is supported.
    pub url: String,
    /// Status codes that mean ready. Empty means any 2xx.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_status: Vec<u16>,
    /// How long between attempts.
    #[serde(default = "default_readiness_interval")]
    pub interval_seconds: u64,
    /// How long a single attempt may take.
    #[serde(default = "default_readiness_timeout")]
    pub timeout_seconds: u64,
    /// Give up waiting after this long and launch anyway. `null` waits forever,
    /// which is usually right for an appliance: showing an error page is worse
    /// than showing the background until the service appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub give_up_after_seconds: Option<u64>,
}

impl ReadinessConfig {
    /// Whether a status code counts as ready.
    pub fn accepts(&self, status: u16) -> bool {
        if self.expect_status.is_empty() {
            (200..300).contains(&status)
        } else {
            self.expect_status.contains(&status)
        }
    }
}

fn default_readiness_interval() -> u64 {
    2
}

fn default_readiness_timeout() -> u64 {
    5
}

impl OutputConfig {
    pub fn new(r#match: OutputMatch) -> Self {
        Self {
            r#match,
            enable: true,
            mode: None,
            position: None,
            geometry: None,
            scale: None,
            transform: None,
            adaptive_sync: false,
            allow_tearing: false,
            max_render_time_ms: None,
            background: None,
            adopted: None,
            arrange_offset: None,
        }
    }

    /// The mode to apply: the operator's own if given, else whatever was
    /// adopted because none was. This is what every reconciliation reader
    /// wants — reading `mode` directly sees only the operator's half of the
    /// story and will look like "the pin did nothing".
    pub fn effective_mode(&self) -> Option<Mode> {
        self.mode
            .or_else(|| self.adopted.as_ref().and_then(|adopted| adopted.mode))
    }

    /// As [`OutputConfig::effective_mode`], for scale.
    pub fn effective_scale(&self) -> Option<f64> {
        self.scale
            .or_else(|| self.adopted.as_ref().and_then(|adopted| adopted.scale))
    }

    /// As [`OutputConfig::effective_mode`], for transform.
    pub fn effective_transform(&self) -> Option<Transform> {
        self.transform
            .or_else(|| self.adopted.as_ref().and_then(|adopted| adopted.transform))
    }

    /// Destination buffer dimensions. Simple presents in compositor logical
    /// coordinates; Warp requires a normal, unit-scale physical raster.
    /// Validate before casting or allocating, including very small scales.
    pub fn presentation_dimensions(
        &self,
        mode: Mode,
        apply_correction: bool,
    ) -> Result<(i32, i32), String> {
        let scale = if apply_correction {
            1.0
        } else {
            self.effective_scale().unwrap_or(1.0)
        };
        if !(scale.is_finite() && scale > 0.0) {
            return Err("output scale must be finite and positive".into());
        }
        let rotated = !apply_correction
            && matches!(
                self.effective_transform(),
                Some(
                    Transform::Rotate90
                        | Transform::Rotate270
                        | Transform::Flipped90
                        | Transform::Flipped270
                )
            );
        let (width, height) = if rotated {
            (mode.height, mode.width)
        } else {
            (mode.width, mode.height)
        };
        let width = (f64::from(width) / scale).round();
        let height = (f64::from(height) / scale).round();
        let max_dimension = f64::from(super::limits::MAX_DIMENSION);
        if !(1.0..=max_dimension).contains(&width) || !(1.0..=max_dimension).contains(&height) {
            return Err("presentation dimensions must be in 1..=32768 pixels per axis".into());
        }
        if width * height > super::limits::MAX_OUTPUT_PIXELS as f64 {
            return Err("presentation allocation exceeds the 32MP limit".into());
        }
        Ok((width as i32, height as i32))
    }
}

/// How an application is launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Launcher {
    /// Chromium with Suede's kiosk argument set.
    #[serde(rename_all = "camelCase")]
    ChromiumKiosk {
        /// URI to load. Supports `{appId}` and `{heartbeatUrl}` placeholders.
        uri: String,
        #[serde(default)]
        show_fps_counter: bool,
        /// Appended after the preset's arguments.
        #[serde(default)]
        extra_args: Vec<String>,
        /// Which binary to launch, overriding the search.
        ///
        /// Suede normally tries `chromium`, `chromium-browser`,
        /// `google-chrome-stable` and `google-chrome` in that order,
        /// preferring any that is not a snap. Name one here to settle it —
        /// a bare name is looked up on `PATH`, a path is used as given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
        /// Grant the page's own origin camera and microphone access in its
        /// private profile, so it can enumerate capture devices and choose
        /// one by name rather than only ever getting whatever device the
        /// browser hands it first. On by default.
        ///
        /// As with the autoplay and capture-prompt bypass above, the operator
        /// choosing what this machine runs is the consent a permission
        /// prompt would otherwise collect.
        #[serde(default = "default_true")]
        grant_capture: bool,
    },
    /// Firefox with its kiosk argument set.
    #[serde(rename_all = "camelCase")]
    FirefoxKiosk {
        uri: String,
        #[serde(default)]
        extra_args: Vec<String>,
        /// Which binary to launch, overriding the search. See
        /// [`Launcher::ChromiumKiosk::program`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
    },
    /// Any executable, launched verbatim.
    #[serde(rename_all = "camelCase")]
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl Launcher {
    /// True for launchers that need a private browser profile directory.
    pub fn is_chromium(&self) -> bool {
        matches!(self, Self::ChromiumKiosk { .. })
    }

    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::ChromiumKiosk { uri, .. } | Self::FirefoxKiosk { uri, .. } => Some(uri),
            Self::Exec { .. } => None,
        }
    }

    /// Executable this launcher needs on `PATH`.
    pub fn program(&self) -> &str {
        match self {
            Self::ChromiumKiosk { .. } => "chromium",
            Self::FirefoxKiosk { .. } => "firefox",
            Self::Exec { command, .. } => command,
        }
    }
}

/// Restart behavior after an application exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicyKind {
    /// Always relaunch, whatever the exit status.
    #[default]
    Always,
    /// Relaunch only on a non-zero exit.
    OnFailure,
    /// Never relaunch automatically.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartPolicy {
    pub policy: RestartPolicyKind,
    /// Initial delay before relaunching.
    pub delay_ms: u64,
    /// Ceiling for the exponential backoff.
    pub max_delay_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            policy: RestartPolicyKind::Always,
            delay_ms: 1000,
            max_delay_ms: 30_000,
        }
    }
}

impl RestartPolicy {
    /// Delay before attempt number `attempt` (1-based), with exponential backoff.
    pub fn delay_for(&self, attempt: u32) -> std::time::Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let scaled = self.delay_ms.saturating_mul(1u64 << shift);
        std::time::Duration::from_millis(scaled.min(self.max_delay_ms.max(self.delay_ms)))
    }

    pub fn should_restart(&self, exit_code: Option<i32>) -> bool {
        match self.policy {
            RestartPolicyKind::Always => true,
            RestartPolicyKind::OnFailure => exit_code != Some(0),
            RestartPolicyKind::Never => false,
        }
    }
}

/// Where an application's audio should go.
///
/// Omitting this field entirely is the third option and a different one:
/// it defers the choice to each launch, so the app follows whatever
/// PipeWire's default sink is at the time. Naming a sink here locks the app
/// to it however the machine's default moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioConfig {
    /// PipeWire `node.name` of the sink to lock this app to. `null` locks it
    /// to silence.
    pub output: Option<String>,
    /// Playback gain for that sink, in dB. `0.0` is unity, and the default.
    ///
    /// Unity is the default because it is the only level that means the same
    /// thing on every machine. A sink left wherever some earlier session put
    /// it passes signal at a level nobody knows, which is a miserable fault to
    /// chase: everything works, quietly. On a digital output it is worse than
    /// inconvenient — every dB taken here is resolution discarded before the
    /// link, and nothing downstream can put it back. Attenuate at the
    /// amplifier instead, and leave this alone.
    ///
    /// The scale runs from `-100.0`, which means silence rather than a very
    /// small gain, up to `0.0`. There is nothing above unity: a digital sink
    /// has no headroom above full scale and can only clip.
    ///
    /// Mind the scales when comparing with other tools. PipeWire's own
    /// `channelVolumes` is linear amplitude; the number `wpctl` prints is its
    /// cube root, so `wpctl`'s 0.40 is -24 dB, not -8.
    #[serde(default)]
    pub gain_db: f64,
}

/// Content-level watchdog settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    /// Silence tolerated once armed, before the app is killed and relaunched.
    #[serde(default = "default_heartbeat_timeout")]
    pub timeout_seconds: u64,
    /// Time allowed after launch for the first heartbeat to arrive.
    #[serde(default = "default_heartbeat_grace")]
    pub startup_grace_seconds: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_seconds: default_heartbeat_timeout(),
            startup_grace_seconds: default_heartbeat_grace(),
        }
    }
}

/// A managed application: a launch specification, not a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppConfig {
    /// Client-chosen, unique, stable identifier.
    pub id: String,
    pub launcher: Launcher,
    /// Whether this app runs. Not part of the API: derived from
    /// [`DesiredState::active_app`] by the reconciler, kept as a field only
    /// because the supervisor consumes per-app configs.
    #[serde(skip)]
    #[schema(ignore)]
    pub enabled: bool,
    /// Where the window goes. Not part of the API: the active app always
    /// covers the whole canvas, and the reconciler decides whether that
    /// canvas is the physical span or a headless output.
    #[serde(skip)]
    #[schema(ignore)]
    pub output: Option<OutputMatch>,
    #[serde(skip)]
    #[schema(ignore)]
    pub fullscreen: bool,
    #[serde(skip)]
    #[schema(ignore)]
    pub span_outputs: bool,
    /// Audio routing. Absent leaves routing untouched; `{"output": null}` silences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioConfig>,
    /// Extra environment variables for the launched process.
    ///
    /// Applied last, so they override anything the launcher preset sets.
    /// Hardware acceleration usually needs this: enabling NVDEC on an Nvidia
    /// card, for instance, is a matter of `LIBVA_DRIVER_NAME` and
    /// `NVD_BACKEND` rather than any command-line flag.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Wait for this URL to answer before launching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ReadinessConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<HeartbeatConfig>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Keep the browser profile between launches instead of wiping it.
    #[serde(default)]
    pub persist_profile: bool,
}

impl AppConfig {
    pub fn watchdog(&self) -> Option<&HeartbeatConfig> {
        self.heartbeat.as_ref().filter(|h| h.enabled)
    }

    /// Whether this app is expected to map a window.
    ///
    /// Browser presets always are. A bare `exec` might be a headless helper, so
    /// it only counts when an output is pinned — placement is the thing that
    /// needs a window.
    pub fn expects_window(&self) -> bool {
        match self.launcher {
            Launcher::ChromiumKiosk { .. } | Launcher::FirefoxKiosk { .. } => true,
            Launcher::Exec { .. } => self.output.is_some(),
        }
    }
}

/// Daemon-level settings that belong to desired state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// Hide the pointer and park it beyond the layout.
    pub hide_cursor: bool,
    /// Backstop poll interval for output changes.
    pub output_poll_interval_seconds: u64,
    /// Accepted and ignored since 0.2.0; the raw-command endpoint is no
    /// longer gated. Kept as a field, never written back, so a document
    /// saved by an older release still loads — `Settings` refuses unknown
    /// fields and there is no migration step. Delete it once no appliance
    /// still holds a document that predates 0.2.0.
    #[serde(default, skip_serializing)]
    pub allow_raw_sway_commands: bool,
    /// Measure browser decode capabilities at startup when the browser,
    /// its configuration, or the GPU driver changed since last measured.
    /// A brief window opens on the displays while it runs; with nothing
    /// changed, nothing opens.
    #[serde(default = "default_true")]
    pub measure_capabilities_on_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hide_cursor: true,
            output_poll_interval_seconds: 5,
            allow_raw_sway_commands: false,
            measure_capabilities_on_start: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_heartbeat_timeout() -> u64 {
    25
}

fn default_heartbeat_grace() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CanvasRect;

    #[test]
    fn complete_simple_and_warp_examples_validate_against_the_current_schema() {
        for json in [
            include_str!("../../docs/examples/four-output-appliance.json"),
            include_str!("../../docs/examples/four-output-warp.json"),
            include_str!("../../docs/examples/four-output-shared-canvas.json"),
            include_str!("../../docs/examples/four-output-adaptive.json"),
        ] {
            let state: DesiredState = serde_json::from_str(json).unwrap();
            assert_eq!(state.schema_version, SCHEMA_VERSION);
            state.validate(true).unwrap();
        }
    }
    use crate::model::observed::Rect;

    fn output(name: &str, make: Option<&str>) -> Output {
        Output {
            name: name.into(),
            active: true,
            make: make.map(str::to_string),
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: Rect::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    #[test]
    fn match_by_name() {
        let rule = OutputMatch::by_name("HDMI-A-1");
        assert!(rule.matches(&output("HDMI-A-1", None)));
        assert!(!rule.matches(&output("HDMI-A-2", None)));
    }

    #[test]
    fn match_by_edid_requires_all_specified_fields() {
        let rule = OutputMatch {
            make: Some("Acme".into()),
            ..Default::default()
        };
        assert!(rule.matches(&output("HDMI-A-1", Some("Acme"))));
        assert!(!rule.matches(&output("HDMI-A-1", Some("Other"))));
        assert!(!rule.matches(&output("HDMI-A-1", None)));
    }

    #[test]
    fn empty_match_never_matches() {
        assert!(!OutputMatch::default().matches(&output("HDMI-A-1", None)));
    }

    #[test]
    fn key_round_trips() {
        for rule in [
            OutputMatch::by_name("HDMI-A-1"),
            OutputMatch {
                make: Some("Acme".into()),
                model: Some("X1".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(OutputMatch::parse_key(&rule.key()), rule);
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let policy = RestartPolicy {
            policy: RestartPolicyKind::Always,
            delay_ms: 1000,
            max_delay_ms: 30_000,
        };
        assert_eq!(policy.delay_for(1).as_millis(), 1000);
        assert_eq!(policy.delay_for(2).as_millis(), 2000);
        assert_eq!(policy.delay_for(3).as_millis(), 4000);
        assert_eq!(policy.delay_for(20).as_millis(), 30_000);
    }

    #[test]
    fn on_failure_ignores_clean_exit() {
        let policy = RestartPolicy {
            policy: RestartPolicyKind::OnFailure,
            ..Default::default()
        };
        assert!(!policy.should_restart(Some(0)));
        assert!(policy.should_restart(Some(1)));
        assert!(policy.should_restart(None));
    }

    /// A15-adjacent: `validate_projection` used to push "projection.canvas
    /// is required in warp mode" once per enabled output sharing the
    /// canvas, rather than once for the document.
    #[test]
    fn missing_canvas_in_warp_mode_is_reported_once_not_per_output() {
        let mut state = DesiredState::new();
        state.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Warp,
            ..Default::default()
        });
        for name in ["A", "B", "C"] {
            let mut output = OutputConfig::new(OutputMatch::by_name(name));
            output.mode = Some(Mode {
                width: 100,
                height: 100,
                refresh_hz: 60.0,
            });
            output.position = Some(Position { x: 0, y: 0 });
            state.outputs.push(output);
        }
        let errors = state.validate(true).unwrap_err();
        let canvas_required = errors
            .iter()
            .filter(|e| e.contains("is required in warp mode"))
            .count();
        assert_eq!(
            canvas_required, 1,
            "one fact about the document, not one per output: {errors:?}"
        );
    }

    /// The general per-output geometry check and the shared-canvas check
    /// used to both call `OutputGeometry::validate_for_output` against the
    /// same canvas and mode for a shared+enabled output, doubling every
    /// geometry error.
    #[test]
    fn invalid_shared_geometry_is_reported_once_not_twice() {
        let mut state = DesiredState::new();
        state.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        let mut output = OutputConfig::new(OutputMatch::by_name("A"));
        output.mode = Some(Mode {
            width: 100,
            height: 100,
            refresh_hz: 60.0,
        });
        output.position = Some(Position { x: 0, y: 0 });
        output.geometry = Some(OutputGeometry {
            // Not finite-positive: fails validate_numbers, which both the
            // general check and the (now removed) duplicate shared-canvas
            // check used to run independently.
            slice: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: -1.0,
                height: 1.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });
        state.outputs.push(output);
        let errors = state.validate(true).unwrap_err();
        let geometry_errors: Vec<_> = errors.iter().filter(|e| e.contains(".geometry")).collect();
        assert_eq!(geometry_errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn validation_catches_duplicate_app_ids() {
        let app = AppConfig {
            id: "a".into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "true".into(),
                args: vec![],
            },
            output: None,
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        };
        let state = DesiredState {
            apps: vec![app.clone(), app],
            ..DesiredState::new()
        };
        let errors = state.validate(false).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("is not unique")));
    }

    fn placed(name: &str, x: i32, y: i32, width: i32, height: i32) -> OutputConfig {
        OutputConfig {
            r#match: OutputMatch::by_name(name),
            enable: true,
            mode: Some(Mode {
                width,
                height,
                refresh_hz: 60.0,
            }),
            position: Some(Position { x, y }),
            geometry: None,
            scale: None,
            transform: None,
            adaptive_sync: false,
            allow_tearing: false,
            max_render_time_ms: None,
            background: None,
            adopted: None,
            arrange_offset: None,
        }
    }

    fn layout(outputs: Vec<OutputConfig>) -> DesiredState {
        DesiredState {
            outputs,
            ..DesiredState::new()
        }
    }

    #[test]
    fn a_layout_chained_by_overlaps_and_edges_is_valid() {
        // A overlaps B; C only shares an edge with B; D reaches A through
        // both of them. Chaining is transitive.
        layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 1760, 0, 1920, 1080),
            placed("C", 3680, 0, 1920, 1080),
            placed("D", 3680, 1080, 1920, 1080),
        ])
        // `true`: overlapping rectangles are only legitimate on an
        // appliance configured for them, and this is the chaining rule they
        // exercise.
        .validate(true)
        .expect("chained layout must validate");
    }

    #[test]
    fn a_gap_in_the_layout_is_rejected() {
        let errors = layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 2000, 0, 1920, 1080),
        ])
        .validate(false)
        .unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("not contiguous")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_corner_touch_is_not_a_chain() {
        // Diagonal neighbors meet in a single point: no pixel row or column
        // crosses between them, so the canvas is still split in two.
        let errors = layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 1920, 1080, 1920, 1080),
        ])
        .validate(false)
        .unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("not contiguous")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_stranded_island_names_its_outputs() {
        // A and B chain; C floats alone. The error must say which.
        let errors = layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 1760, 0, 1920, 1080),
            placed("C", 10_000, 0, 1920, 1080),
        ])
        .validate(true)
        .unwrap_err();
        let message = errors
            .iter()
            .find(|e| e.contains("not contiguous"))
            .expect("contiguity error");
        assert!(
            message.contains("C cannot be chained back to A"),
            "{message}"
        );
        assert!(!message.contains('B'), "{message}");
    }

    #[test]
    fn an_overlapping_pair_is_rejected_on_a_tiling_appliance() {
        let errors = layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 1760, 0, 1920, 1080),
        ])
        .validate(false)
        .unwrap_err();
        let message = errors
            .iter()
            .find(|e| e.contains("overlap"))
            .expect("overlap error");
        // Both names, because either one of them could be the one in the
        // wrong place and the writer is the only one who knows which.
        assert!(message.contains('A') && message.contains('B'), "{message}");
        assert!(message.contains("160x1080"), "{message}");
    }

    #[test]
    fn an_overlapping_pair_is_accepted_when_overlaps_are_allowed() {
        layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 1760, 0, 1920, 1080),
        ])
        .validate(true)
        .expect("an overlap is the projection configuration on such a machine");
    }

    /// A configured canvas used to exempt every output from the plain
    /// integer-position overlap check, not just the ones that actually
    /// carry their own slice rectangle.
    #[test]
    fn a_canvas_only_exempts_outputs_that_carry_their_own_geometry_from_the_overlap_check() {
        let mut a = placed("A", 0, 0, 1920, 1080);
        a.geometry = Some(OutputGeometry {
            slice: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });
        // B overlaps A by integer position and carries no geometry of its
        // own.
        let mut state = layout(vec![a, placed("B", 1760, 0, 1920, 1080)]);
        state.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Simple,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        let errors = state.validate(false).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("overlap")),
            "B has no slice rectangle of its own and must still be checked \
             even though a canvas is configured elsewhere: {errors:?}"
        );
    }

    #[test]
    fn a_shared_edge_is_not_an_overlap() {
        // Edge to edge is exactly what a tiling appliance wants: the
        // rectangles touch along a line and share no pixel.
        layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 1920, 0, 1920, 1080),
        ])
        .validate(false)
        .expect("a tiled layout must validate");
    }

    #[test]
    fn every_overlapping_pair_is_named_not_just_the_first() {
        // Three stacked outputs overlap pairwise in three different ways, and
        // a writer fixing one of them should not have to submit again to
        // discover the next.
        let errors = layout(vec![
            placed("A", 0, 0, 1920, 1080),
            placed("B", 100, 0, 1920, 1080),
            placed("C", 200, 0, 1920, 1080),
        ])
        .validate(false)
        .unwrap_err();
        assert_eq!(errors.iter().filter(|e| e.contains("overlap")).count(), 3);
    }

    #[test]
    fn outputs_without_a_pinned_rectangle_are_not_checked() {
        // No configured mode or position: geometry comes from observation at
        // reconcile time, so the document alone cannot condemn it.
        let mut floating = placed("B", 9_999, 0, 1920, 1080);
        floating.position = None;
        layout(vec![placed("A", 0, 0, 1920, 1080), floating])
            .validate(false)
            .expect("unpinned outputs are exempt");
    }

    #[test]
    fn disabled_outputs_do_not_bridge_a_gap() {
        // The disabled middle output would chain A to C if it counted — but
        // it emits no pixels, so the gap is real.
        let mut bridge = placed("B", 1800, 0, 1920, 1080);
        bridge.enable = false;
        let errors = layout(vec![
            placed("A", 0, 0, 1920, 1080),
            bridge,
            placed("C", 3600, 0, 1920, 1080),
        ])
        .validate(false)
        .unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("not contiguous")),
            "{errors:?}"
        );
    }

    #[test]
    fn validation_rejects_bad_environment_names() {
        let mut app = AppConfig {
            id: "a".into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "true".into(),
                args: vec![],
            },
            output: None,
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        };
        app.env.insert("BAD=NAME".into(), "x".into());
        let state = DesiredState {
            apps: vec![app],
            ..DesiredState::new()
        };
        let errors = state.validate(false).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("invalid variable name")));
    }

    #[test]
    fn validation_rejects_unsafe_app_id() {
        let state = DesiredState {
            apps: vec![AppConfig {
                id: "../escape".into(),
                enabled: true,
                launcher: Launcher::Exec {
                    command: "true".into(),
                    args: vec![],
                },
                output: None,
                fullscreen: true,
                span_outputs: false,
                env: Default::default(),
                readiness: None,
                audio: None,
                heartbeat: None,
                restart: RestartPolicy::default(),
                persist_profile: false,
            }],
            ..DesiredState::new()
        };
        assert!(state.validate(false).is_err());
    }

    #[test]
    fn launcher_round_trips_through_json() {
        let launcher = Launcher::ChromiumKiosk {
            uri: "http://example.com".into(),
            show_fps_counter: true,
            extra_args: vec!["--mute-audio".into()],
            program: None,
            grant_capture: true,
        };
        let json = serde_json::to_value(&launcher).unwrap();
        assert_eq!(json["kind"], "chromium-kiosk");
        assert_eq!(json["showFpsCounter"], true);
        let back: Launcher = serde_json::from_value(json).unwrap();
        assert_eq!(back, launcher);
    }

    #[test]
    fn grant_capture_defaults_to_true_when_absent() {
        // A document written before this field existed, or one an operator
        // wrote by hand without it, must still get the grant — it is meant to
        // be on unless someone deliberately turns it off.
        let json = serde_json::json!({
            "kind": "chromium-kiosk",
            "uri": "http://example.com",
        });
        let launcher: Launcher = serde_json::from_value(json).unwrap();
        assert_eq!(
            launcher,
            Launcher::ChromiumKiosk {
                uri: "http://example.com".into(),
                show_fps_counter: false,
                extra_args: vec![],
                program: None,
                grant_capture: true,
            }
        );
    }

    #[test]
    fn grant_capture_false_round_trips() {
        let launcher = Launcher::ChromiumKiosk {
            uri: "http://example.com".into(),
            show_fps_counter: false,
            extra_args: vec![],
            program: None,
            grant_capture: false,
        };
        let json = serde_json::to_value(&launcher).unwrap();
        assert_eq!(json["grantCapture"], false);
        let back: Launcher = serde_json::from_value(json).unwrap();
        assert_eq!(back, launcher);
    }

    #[test]
    fn audio_absent_and_null_are_distinguishable() {
        let absent: AppConfig =
            serde_json::from_str(r#"{"id":"a","launcher":{"kind":"exec","command":"true"}}"#)
                .unwrap();
        assert!(absent.audio.is_none());
        let silent: AppConfig = serde_json::from_str(
            r#"{"id":"a","launcher":{"kind":"exec","command":"true"},"audio":{"output":null}}"#,
        )
        .unwrap();
        assert_eq!(
            silent.audio,
            Some(AudioConfig {
                output: None,
                gain_db: 0.0
            })
        );
    }

    // --- background presets ----------------------------------------------

    #[test]
    fn a_bare_string_background_is_a_preset_reference() {
        // The shorthand is what the UI writes, so it has to survive a round
        // trip exactly; an object must still parse as inline properties.
        let reference: BackgroundRef = serde_json::from_str(r#""lobby""#).unwrap();
        assert_eq!(reference, BackgroundRef::Preset("lobby".into()));
        assert_eq!(serde_json::to_string(&reference).unwrap(), r#""lobby""#);

        let inline: BackgroundRef =
            serde_json::from_str(r#"{"wallpaper":"art","mode":"fit"}"#).unwrap();
        assert!(matches!(inline, BackgroundRef::Inline(_)));
        assert_eq!(inline.preset_id(), None);
    }

    #[test]
    fn a_preset_flattens_its_properties() {
        // `{"id":..,"wallpaper":..}`, not `{"id":..,"background":{..}}` — the
        // nesting would be visible in every hand-written config file.
        let preset: BackgroundPreset =
            serde_json::from_str(r##"{"id":"lobby","wallpaper":"art","color":"#101820"}"##)
                .unwrap();
        assert_eq!(preset.id, "lobby");
        assert_eq!(preset.background.wallpaper.as_deref(), Some("art"));
        let text = serde_json::to_string(&preset).unwrap();
        assert!(text.contains(r#""id":"lobby""#), "{text}");
        assert!(!text.contains("background"), "{text}");
    }

    #[test]
    fn an_output_naming_an_undefined_preset_is_rejected() {
        let mut state = DesiredState::new();
        let mut output = OutputConfig::new(OutputMatch {
            name: Some("HDMI-A-1".into()),
            ..Default::default()
        });
        output.background = Some(BackgroundRef::Preset("nope".into()));
        state.outputs.push(output);

        let errors = state.validate(false).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("not a defined background preset")),
            "{errors:?}"
        );
    }

    #[test]
    fn preset_ids_must_be_unique_and_their_colors_valid() {
        let mut state = DesiredState::new();
        state.backgrounds.push(BackgroundPreset {
            id: "one".into(),
            background: Background {
                color: Some("not-a-color".into()),
                ..Default::default()
            },
        });
        state.backgrounds.push(BackgroundPreset {
            id: "one".into(),
            ..Default::default()
        });

        let errors = state.validate(false).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("is not unique")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("#rrggbb")), "{errors:?}");
    }

    #[test]
    fn an_unset_color_resolves_to_black() {
        assert_eq!(Background::default().sway_color(), "000000");
        assert_eq!(
            Background {
                color: Some("#ABCDEF".into()),
                ..Default::default()
            }
            .sway_color(),
            "ABCDEF"
        );
    }

    #[test]
    fn a_reference_resolves_through_the_preset_table() {
        let presets = vec![BackgroundPreset {
            id: "lobby".into(),
            background: Background {
                wallpaper: Some("art".into()),
                ..Default::default()
            },
        }];
        let found = BackgroundRef::Preset("lobby".into());
        assert_eq!(
            found.resolve(&presets).unwrap().wallpaper.as_deref(),
            Some("art")
        );
        assert!(BackgroundRef::Preset("gone".into())
            .resolve(&presets)
            .is_none());
    }

    // --- adopted output values ---------------------------------------------

    #[test]
    fn effective_fields_prefer_the_operators_own_value() {
        let mut output = OutputConfig::new(OutputMatch::by_name("HDMI-A-1"));
        output.mode = Some(Mode {
            width: 1920,
            height: 1200,
            refresh_hz: 59.95,
        });
        output.adopted = Some(AdoptedOutput {
            mode: Some(Mode {
                width: 3840,
                height: 2160,
                refresh_hz: 60.0,
            }),
            scale: Some(2.0),
            transform: Some(Transform::Rotate90),
            display: None,
            captured_at: 0,
        });

        // The operator's own mode wins even though a different one is
        // adopted; scale and transform, which the operator never set, fall
        // through to the adopted value.
        assert_eq!(output.effective_mode(), output.mode);
        assert_eq!(output.effective_scale(), Some(2.0));
        assert_eq!(output.effective_transform(), Some(Transform::Rotate90));
    }

    #[test]
    fn effective_fields_fall_back_to_nothing_when_neither_is_set() {
        let output = OutputConfig::new(OutputMatch::by_name("HDMI-A-1"));
        assert_eq!(output.effective_mode(), None);
        assert_eq!(output.effective_scale(), None);
        assert_eq!(output.effective_transform(), None);
    }

    #[test]
    fn display_identity_is_none_without_any_edid_field() {
        assert_eq!(DisplayIdentity::of(&output("HDMI-A-1", None)), None);
        assert_eq!(
            DisplayIdentity::of(&output("HDMI-A-1", Some("Acme"))),
            Some(DisplayIdentity {
                make: Some("Acme".into()),
                model: None,
                serial: None,
            })
        );
    }

    #[test]
    fn transform_survives_the_round_trip_through_sways_own_spelling() {
        for transform in [
            Transform::Normal,
            Transform::Rotate90,
            Transform::Rotate180,
            Transform::Rotate270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            assert_eq!(Transform::from_sway(transform.as_sway()), Some(transform));
        }
        assert_eq!(Transform::from_sway("sideways"), None);
    }

    #[test]
    fn an_adopted_mode_counts_toward_the_contiguity_check() {
        // Adopting fills in geometry the operator never typed, and a
        // validator that only looked at the operator's own field would
        // wrongly stop enforcing contiguity the moment a mode is pinned.
        let mut second = placed("B", 1760, 0, 1920, 1080);
        second.mode = None;
        second.adopted = Some(AdoptedOutput {
            mode: Some(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0,
            }),
            scale: None,
            transform: None,
            display: None,
            captured_at: 0,
        });
        layout(vec![placed("A", 0, 0, 1920, 1080), second])
            .validate(true)
            .expect("the adopted mode should still chain to A");
    }
}
