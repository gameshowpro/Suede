//! Pure source measurement and adaptive black-lift control.
//!
//! The reduction is intentionally source-space and deterministic: at most a
//! 256 by 256 regular grid is sampled once for a capture. The active canvas
//! dimensions and row stride are supplied separately, so allocation padding
//! is never mistaken for picture content.

use std::time::{Duration, Instant};

use crate::model::AdaptiveBlackLift;

/// Maximum number of source samples along either grid axis.
pub const MAX_GRID_AXIS: usize = 256;
/// Differences below this level are treated as settled.
pub const SETTLE_EPSILON: f64 = 1.0e-5;
/// Suggested repaint cadence while the controller is moving.
pub const CONTROLLER_TICK: Duration = Duration::from_millis(20);

/// A source-canvas luminance reduction result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LuminanceMeasurement {
    pub mean: f64,
    pub sample_count: u64,
}

/// The regular source-space sample grid used by [`measure_rgb8`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleGrid {
    pub width: usize,
    pub height: usize,
    pub columns: usize,
    pub rows: usize,
}

impl SampleGrid {
    /// Make a grid covering the complete active canvas.
    pub fn new(width: usize, height: usize) -> Option<Self> {
        (width != 0 && height != 0).then(|| Self {
            width,
            height,
            columns: width.min(MAX_GRID_AXIS),
            rows: height.min(MAX_GRID_AXIS),
        })
    }

    /// Map a grid coordinate to the nearest source pixel center.
    pub fn pixel(self, column: usize, row: usize) -> (usize, usize) {
        assert!(column < self.columns && row < self.rows);
        // `((2*i+1)*n)/(2*m)` is floor((i + 0.5) * n / m), with integer
        // arithmetic and no floating-point rounding at large resolutions.
        let x = ((2 * column + 1) * self.width / (2 * self.columns)).min(self.width - 1);
        let y = ((2 * row + 1) * self.height / (2 * self.rows)).min(self.height - 1);
        (x, y)
    }

    /// Number of locations sampled in one capture.
    pub fn sample_count(self) -> u64 {
        (self.columns * self.rows) as u64
    }
}

/// Decode one sRGB byte to linear signal.
pub fn srgb_to_linear(byte: u8) -> f64 {
    let value = f64::from(byte) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Compute Rec. 709 luminance from an sRGB byte triplet.
pub fn rgb8_luminance(rgb: [u8; 3]) -> f64 {
    0.2126 * srgb_to_linear(rgb[0])
        + 0.7152 * srgb_to_linear(rgb[1])
        + 0.0722 * srgb_to_linear(rgb[2])
}

/// Measure an RGB8 canvas on a regular grid of pixel centers.
///
/// `width` and `height` describe active canvas pixels. `stride_bytes` may be
/// larger than `width * 3`, as it often is for a mapped image; bytes after the
/// active row are ignored. Black or otherwise unused canvas locations inside
/// the active dimensions are included by design. A malformed/empty image
/// returns `None` rather than making a partial measurement. This is a point
/// statistic: a bright feature narrower than a grid cell can alias away, so
/// it is suitable for stable scene darkness rather than an area-exact meter.
pub fn measure_rgb8(
    bytes: &[u8],
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> Option<LuminanceMeasurement> {
    let grid = SampleGrid::new(width, height)?;
    let active_row_bytes = width.checked_mul(3)?;
    if stride_bytes < active_row_bytes {
        return None;
    }
    let required = stride_bytes.checked_mul(height)?;
    if bytes.len() < required {
        return None;
    }

    let mut sum = 0.0;
    for row in 0..grid.rows {
        for column in 0..grid.columns {
            let (x, y) = grid.pixel(column, row);
            let offset = y * stride_bytes + x * 3;
            sum += rgb8_luminance([bytes[offset], bytes[offset + 1], bytes[offset + 2]]);
        }
    }
    Some(LuminanceMeasurement {
        mean: sum / grid.sample_count() as f64,
        sample_count: grid.sample_count(),
    })
}

/// Compute the target lift from a measured linear luminance.
pub fn target_for_luminance(config: AdaptiveBlackLift, luminance: f64) -> f64 {
    if !luminance.is_finite() {
        return config.level;
    }
    let ratio = ((config.bright_threshold - luminance)
        / (config.bright_threshold - config.dark_threshold))
        .clamp(0.0, 1.0);
    (config.level * ratio).clamp(0.0, config.level)
}

/// Why the current controller value is being used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerStatus {
    /// No valid capture has arrived; the configured fixed level is used.
    Startup,
    /// A valid capture supplied the current target.
    Valid,
    /// A zero-count capture retained the previous target.
    Stale,
    /// Measurement is unavailable and the configured fixed level is used.
    Unavailable,
    /// Adaptation is suspended for a calibration pattern.
    Paused,
}

/// Result of one monotonic controller tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControllerTick {
    pub level: f64,
    pub target: f64,
    pub changed: bool,
    pub settled: bool,
    pub status: ControllerStatus,
    pub elapsed: Duration,
}

/// Monotonic adaptive lift controller shared by all outputs in a generation.
#[derive(Debug, Clone)]
pub struct AdaptiveController {
    config: AdaptiveBlackLift,
    level: f64,
    target: f64,
    last_tick: Instant,
    status: ControllerStatus,
    paused: bool,
    last_capture_id: Option<u64>,
}

impl AdaptiveController {
    /// Start at the configured fixed level until a valid measurement arrives.
    pub fn new(config: AdaptiveBlackLift, started_at: Instant) -> Self {
        Self {
            config,
            level: config.level,
            target: config.level,
            last_tick: started_at,
            status: ControllerStatus::Startup,
            paused: false,
            last_capture_id: None,
        }
    }

    pub fn config(&self) -> AdaptiveBlackLift {
        self.config
    }

    pub fn level(&self) -> f64 {
        self.level
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    pub fn status(&self) -> ControllerStatus {
        self.status
    }

    pub fn last_capture_id(&self) -> Option<u64> {
        self.last_capture_id
    }

    /// Supply a measurement. Zero samples retain the last target and report
    /// stale data; they must not turn a temporary empty reduction into black.
    pub fn measure(&mut self, luminance: f64, sample_count: u64) {
        self.measure_with_capture(luminance, sample_count, None);
    }

    /// As [`Self::measure`], while retaining the source capture identity.
    pub fn measure_with_capture(
        &mut self,
        luminance: f64,
        sample_count: u64,
        capture_id: Option<u64>,
    ) {
        if sample_count == 0 || !luminance.is_finite() {
            if !self.paused {
                self.status = ControllerStatus::Stale;
            }
            return;
        }
        self.target = target_for_luminance(self.config, luminance);
        self.last_capture_id = capture_id;
        if !self.paused {
            self.status = ControllerStatus::Valid;
        }
    }

    /// Fall back immediately to configured fixed lift when measurement is not
    /// available. This also makes a fallback settle without a repaint timer.
    pub fn unavailable(&mut self) {
        self.target = self.config.level;
        self.level = self.config.level;
        self.status = ControllerStatus::Unavailable;
    }

    /// Suspend/resume adaptation for a calibration pattern. Resetting the
    /// monotonic anchor on both edges prevents paused time becoming a giant dt.
    pub fn set_paused(&mut self, paused: bool, now: Instant) {
        self.last_tick = now;
        self.paused = paused;
        if paused {
            self.level = self.config.level;
            self.status = ControllerStatus::Paused;
        } else if self.status == ControllerStatus::Paused {
            self.status = ControllerStatus::Valid;
        }
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Advance using monotonic elapsed time, applying smoothing then slew.
    pub fn tick(&mut self, now: Instant) -> ControllerTick {
        let elapsed = now
            .checked_duration_since(self.last_tick)
            .unwrap_or_default();
        self.last_tick = now;
        let before = self.level;
        if self.paused {
            self.level = self.config.level;
        } else if self.status != ControllerStatus::Unavailable {
            let dt = elapsed.as_secs_f64();
            if dt > 0.0 {
                let tau = if self.target > self.level {
                    self.config.rise_seconds()
                } else {
                    self.config.fall_seconds()
                };
                let alpha = 1.0 - (-dt / tau).exp();
                let smoothed = self.level + alpha * (self.target - self.level);
                let max_delta = self.config.slew_per_second * dt;
                self.level = self.level + (smoothed - self.level).clamp(-max_delta, max_delta);
                // Snapping is an exact-settle optimization, but it must still
                // obey the per-step slew bound for tiny irregular ticks.
                if (self.target - self.level).abs() <= SETTLE_EPSILON
                    && (self.target - before).abs() <= max_delta
                {
                    self.level = self.target;
                }
            }
        }
        self.level = self.level.clamp(0.0, self.config.level);
        ControllerTick {
            level: self.level,
            target: self.target,
            changed: (self.level - before).abs() > f64::EPSILON,
            settled: self.status != ControllerStatus::Startup
                && (self.level - self.target).abs() <= SETTLE_EPSILON,
            status: self.status,
            elapsed,
        }
    }

    /// Whether a bounded repaint timer is still useful.
    pub fn needs_tick(&self) -> bool {
        !self.paused
            && self.status != ControllerStatus::Unavailable
            && (self.level - self.target).abs() > SETTLE_EPSILON
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AdaptiveBlackLift {
        AdaptiveBlackLift {
            level: 0.4,
            dark_threshold: 0.02,
            bright_threshold: 0.2,
            rise_ms: 1_000.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        }
    }

    #[test]
    fn metric_decodes_srgb_and_ignores_row_padding() {
        let bytes = [128, 128, 128, 99, 98, 97, 128, 128, 128, 1, 2, 3];
        let result = measure_rgb8(&bytes, 1, 2, 6).unwrap();
        let expected = srgb_to_linear(128);
        assert_eq!(result.sample_count, 2);
        assert!((result.mean - expected).abs() < 1e-12);
    }

    #[test]
    fn metric_uses_full_canvas_and_caps_grid() {
        let white = vec![255_u8; 300 * 300 * 3];
        let result = measure_rgb8(&white, 300, 300, 300 * 3).unwrap();
        assert_eq!(result.sample_count, 256 * 256);
        assert!((result.mean - 1.0).abs() < 1e-12);
        assert_eq!(SampleGrid::new(8, 8).unwrap().sample_count(), 64);
    }

    #[test]
    fn checkerboard_and_thin_line_sampling_is_deterministic() {
        let mut checker = vec![0_u8; 8 * 8 * 3];
        for y in 0..8 {
            for x in 0..8 {
                if (x + y) % 2 == 0 {
                    checker[(y * 8 + x) * 3..(y * 8 + x + 1) * 3].fill(255);
                }
            }
        }
        let checker_result = measure_rgb8(&checker, 8, 8, 24).unwrap();
        assert!((checker_result.mean - 0.5).abs() < 1e-12);

        let mut line = vec![0_u8; 16 * 16 * 3];
        for y in 0..16 {
            for x in 0..16 {
                if x == 3 {
                    line[(y * 16 + x) * 3..(y * 16 + x + 1) * 3].fill(255);
                }
            }
        }
        let line_result = measure_rgb8(&line, 16, 16, 48).unwrap();
        // A one-pixel feature is represented exactly when the grid is dense;
        // this is a useful aliasing fixture for lower-resolution grids.
        assert!((line_result.mean - 1.0 / 16.0).abs() < 1e-12);

        // At 512 pixels the grid is capped at 256 points. A line on an
        // unvisited pixel center may therefore alias away by definition.
        let mut high_res_line = vec![0_u8; 512 * 512 * 3];
        for y in 0..512 {
            high_res_line[(y * 512) * 3..(y * 512 + 1) * 3].fill(255);
        }
        let aliased = measure_rgb8(&high_res_line, 512, 512, 512 * 3).unwrap();
        assert_eq!(aliased.mean, 0.0);
    }

    #[test]
    fn controller_starts_fixed_then_smooths_with_slew_and_irregular_dt() {
        let start = Instant::now();
        let mut controller = AdaptiveController::new(config(), start);
        assert_eq!(controller.level(), 0.4);
        controller.measure(1.0, 64);
        let first = controller.tick(start + Duration::from_millis(17));
        assert!(first.changed);
        assert!(first.level <= 0.4);
        assert!(first.level <= 0.4 + 0.1 * 0.017 + 1e-12);
        let second = controller.tick(start + Duration::from_secs(3));
        // Irregular elapsed time is measured directly, while the slew limit
        // still bounds the total movement in this tick.
        assert!(second.level > 0.0);
        assert!(second.level <= first.level - 0.1 * 2.983 + 1e-12);
        assert!(second.elapsed >= Duration::from_secs(2));
    }

    #[test]
    fn zero_samples_retain_target_and_unavailable_falls_back() {
        let start = Instant::now();
        let mut controller = AdaptiveController::new(config(), start);
        controller.measure(0.0, 10);
        let target = controller.target();
        controller.measure(1.0, 0);
        assert_eq!(controller.target(), target);
        assert_eq!(controller.status(), ControllerStatus::Stale);
        controller.unavailable();
        assert_eq!(controller.level(), config().level);
        assert!(!controller.needs_tick());
    }

    #[test]
    fn pause_and_resume_do_not_apply_paused_time() {
        let start = Instant::now();
        let mut controller = AdaptiveController::new(config(), start);
        controller.measure(0.0, 10);
        controller.tick(start + Duration::from_millis(100));
        controller.set_paused(true, start + Duration::from_millis(100));
        assert_eq!(controller.level(), config().level);
        controller.set_paused(false, start + Duration::from_secs(100));
        let tick = controller.tick(start + Duration::from_secs(100));
        assert_eq!(tick.elapsed, Duration::ZERO);
        assert_eq!(tick.level, config().level);
    }

    #[test]
    fn target_clamps_at_thresholds_and_controller_settles() {
        let config = config();
        assert_eq!(target_for_luminance(config, 0.0), config.level);
        assert_eq!(target_for_luminance(config, 1.0), 0.0);
        let start = Instant::now();
        let mut controller = AdaptiveController::new(config, start);
        controller.measure(1.0, 1);
        let mut now = start;
        for _ in 0..200 {
            now += Duration::from_millis(50);
            if controller.tick(now).settled {
                break;
            }
        }
        assert!(controller.tick(now).settled);
        assert!(!controller.needs_tick());
    }
}
