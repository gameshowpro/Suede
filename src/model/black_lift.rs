//! Public black-level compensation configuration.
//!
//! A number is deliberately retained as the wire representation for fixed
//! lift.  The tagged form leaves room for adaptive control without changing
//! the meaning of an existing configuration:
//!
//! ```json
//! 0.04
//! {"mode":"fixed","level":0.04}
//! {"mode":"adaptive","level":0.2,"darkThreshold":0.02,
//!  "brightThreshold":0.2,"riseMs":1000,"fallMs":250,
//!  "slewPerSecond":0.1}
//! ```

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The largest fixed or adaptive compensation level accepted by the schema.
pub const MAX_BLACK_LIFT: f64 = 0.5;
/// Default luminance below which adaptive compensation approaches its maximum.
pub const DEFAULT_DARK_THRESHOLD: f64 = 0.02;
/// Default luminance above which adaptive compensation reaches zero.
pub const DEFAULT_BRIGHT_THRESHOLD: f64 = 0.2;
/// Default time constant for a rising compensation level, in milliseconds.
pub const DEFAULT_RISE_MS: f64 = 1_000.0;
/// Default time constant for a falling compensation level, in milliseconds.
pub const DEFAULT_FALL_MS: f64 = 250.0;
/// Default maximum level change per second.
pub const DEFAULT_SLEW_PER_SECOND: f64 = 0.1;
/// Time constants smaller than this are not useful for a display controller.
pub const MIN_TIME_CONSTANT_MS: f64 = 1.0;
/// Prevent a typo from making a controller effectively never settle.
pub const MAX_TIME_CONSTANT_MS: f64 = 3_600_000.0;
/// A positive slew is required; this upper bound still permits a fast scene cut.
pub const MAX_SLEW_PER_SECOND: f64 = 10.0;

/// Black lift's backwards-compatible number or its explicitly tagged form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum BlackLift {
    /// A bare number has always meant fixed lift.
    Number(f64),
    /// A tagged fixed or adaptive configuration.
    Tagged(BlackLiftMode),
}

impl Default for BlackLift {
    fn default() -> Self {
        Self::Number(0.0)
    }
}

/// Explicit black-lift modes. The tag is the JSON `mode` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum BlackLiftMode {
    /// Fixed lift using the same arithmetic as the legacy numeric form.
    Fixed { level: f64 },
    /// Lift controlled by source-canvas luminance.
    Adaptive {
        level: f64,
        #[serde(rename = "darkThreshold", default = "default_dark_threshold")]
        dark_threshold: f64,
        #[serde(rename = "brightThreshold", default = "default_bright_threshold")]
        bright_threshold: f64,
        #[serde(rename = "riseMs", default = "default_rise_ms")]
        rise_ms: f64,
        #[serde(rename = "fallMs", default = "default_fall_ms")]
        fall_ms: f64,
        #[serde(rename = "slewPerSecond", default = "default_slew_per_second")]
        slew_per_second: f64,
    },
}

fn default_dark_threshold() -> f64 {
    DEFAULT_DARK_THRESHOLD
}

fn default_bright_threshold() -> f64 {
    DEFAULT_BRIGHT_THRESHOLD
}

fn default_rise_ms() -> f64 {
    DEFAULT_RISE_MS
}

fn default_fall_ms() -> f64 {
    DEFAULT_FALL_MS
}

fn default_slew_per_second() -> f64 {
    DEFAULT_SLEW_PER_SECOND
}

impl BlackLift {
    /// Return the configured maximum/fixed level.
    pub fn level(&self) -> f64 {
        match self {
            Self::Number(level) => *level,
            Self::Tagged(mode) => mode.level(),
        }
    }

    /// Return adaptive parameters, or `None` for fixed configurations.
    pub fn adaptive(&self) -> Option<AdaptiveBlackLift> {
        self.adaptive_settings()
    }

    /// Whether this configuration asks for adaptive control.
    pub fn is_adaptive(&self) -> bool {
        self.adaptive().is_some()
    }

    /// Return the explicit mode, if the caller needs to distinguish fixed
    /// tagged configuration from the legacy numeric spelling.
    pub fn mode(&self) -> BlackLiftModeKind {
        if self.is_adaptive() {
            BlackLiftModeKind::Adaptive
        } else {
            BlackLiftModeKind::Fixed
        }
    }

    /// Return adaptive parameters for constructing a runtime controller.
    pub fn adaptive_settings(&self) -> Option<AdaptiveBlackLift> {
        match self {
            Self::Tagged(BlackLiftMode::Adaptive {
                level,
                dark_threshold,
                bright_threshold,
                rise_ms,
                fall_ms,
                slew_per_second,
            }) => Some(AdaptiveBlackLift {
                level: *level,
                dark_threshold: *dark_threshold,
                bright_threshold: *bright_threshold,
                rise_ms: *rise_ms,
                fall_ms: *fall_ms,
                slew_per_second: *slew_per_second,
            }),
            _ => None,
        }
    }

    /// Validate all numeric bounds, returning a client-facing explanation.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Number(level) => validate_level(*level),
            Self::Tagged(mode) => mode.validate(),
        }
    }
}

impl BlackLiftMode {
    /// Return the configured maximum/fixed level.
    pub fn level(&self) -> f64 {
        match self {
            Self::Fixed { level } | Self::Adaptive { level, .. } => *level,
        }
    }

    /// Validate all numeric bounds, returning a client-facing explanation.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Fixed { level } => validate_level(*level),
            Self::Adaptive {
                level,
                dark_threshold,
                bright_threshold,
                rise_ms,
                fall_ms,
                slew_per_second,
            } => {
                validate_level(*level)?;
                if !dark_threshold.is_finite()
                    || !bright_threshold.is_finite()
                    || !(0.0..=1.0).contains(dark_threshold)
                    || !(0.0..=1.0).contains(bright_threshold)
                    || dark_threshold >= bright_threshold
                {
                    return Err(format!(
                        "adaptive blackLift thresholds must satisfy 0 <= darkThreshold < brightThreshold <= 1, not {dark_threshold} and {bright_threshold}"
                    ));
                }
                validate_time_constant(*rise_ms, "riseMs")?;
                validate_time_constant(*fall_ms, "fallMs")?;
                if !slew_per_second.is_finite()
                    || !(0.0..=MAX_SLEW_PER_SECOND).contains(slew_per_second)
                    || *slew_per_second == 0.0
                {
                    return Err(format!(
                        "adaptive blackLift slewPerSecond must be finite and greater than 0 and at most {MAX_SLEW_PER_SECOND}, not {slew_per_second}"
                    ));
                }
                Ok(())
            }
        }
    }
}

fn validate_level(level: f64) -> Result<(), String> {
    if level.is_finite() && (0.0..=MAX_BLACK_LIFT).contains(&level) {
        Ok(())
    } else {
        Err(format!(
            "blackLift level must be finite and between 0.0 and {MAX_BLACK_LIFT}, not {level}"
        ))
    }
}

fn validate_time_constant(value: f64, field: &str) -> Result<(), String> {
    if value.is_finite() && (MIN_TIME_CONSTANT_MS..=MAX_TIME_CONSTANT_MS).contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "adaptive blackLift {field} must be finite and between {MIN_TIME_CONSTANT_MS} and {MAX_TIME_CONSTANT_MS} milliseconds, not {value}"
        ))
    }
}

/// A compact mode indicator suitable for status and controller selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlackLiftModeKind {
    Fixed,
    Adaptive,
}

/// Runtime parameters for adaptive black lift.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdaptiveBlackLift {
    pub level: f64,
    pub dark_threshold: f64,
    pub bright_threshold: f64,
    pub rise_ms: f64,
    pub fall_ms: f64,
    pub slew_per_second: f64,
}

impl AdaptiveBlackLift {
    /// Validate the runtime adaptive settings carried across the slicer
    /// process boundary. Keep these bounds identical to the tagged public
    /// configuration so an internally constructed spec cannot bypass them.
    pub fn validate(self) -> Result<(), String> {
        validate_level(self.level)?;
        if !self.dark_threshold.is_finite()
            || !self.bright_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.dark_threshold)
            || !(0.0..=1.0).contains(&self.bright_threshold)
            || self.dark_threshold >= self.bright_threshold
        {
            return Err(format!(
                "adaptive blackLift thresholds must satisfy 0 <= darkThreshold < brightThreshold <= 1, not {} and {}",
                self.dark_threshold, self.bright_threshold
            ));
        }
        validate_time_constant(self.rise_ms, "riseMs")?;
        validate_time_constant(self.fall_ms, "fallMs")?;
        if !self.slew_per_second.is_finite()
            || !(0.0..=MAX_SLEW_PER_SECOND).contains(&self.slew_per_second)
            || self.slew_per_second == 0.0
        {
            return Err(format!(
                "adaptive blackLift slewPerSecond must be finite and between greater than 0 and {MAX_SLEW_PER_SECOND}, not {}",
                self.slew_per_second
            ));
        }
        Ok(())
    }

    /// Convert the selected time constant to seconds.
    pub fn rise_seconds(self) -> f64 {
        self.rise_ms / 1_000.0
    }

    /// Convert the selected time constant to seconds.
    pub fn fall_seconds(self) -> f64 {
        self.fall_ms / 1_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_form_round_trips_without_changing_fixed_semantics() {
        let lift: BlackLift = serde_json::from_str("0.04").unwrap();
        assert_eq!(lift, BlackLift::Number(0.04));
        assert_eq!(lift.level(), 0.04);
        assert!(!lift.is_adaptive());
        assert_eq!(serde_json::to_string(&lift).unwrap(), "0.04");
    }

    #[test]
    fn tagged_adaptive_defaults_and_round_trips() {
        let lift: BlackLift = serde_json::from_str(
            r#"{"mode":"adaptive","level":0.2,"darkThreshold":0.03,"brightThreshold":0.3}"#,
        )
        .unwrap();
        let settings = lift.adaptive().unwrap();
        assert_eq!(settings.level, 0.2);
        assert_eq!(settings.dark_threshold, 0.03);
        assert_eq!(settings.bright_threshold, 0.3);
        assert_eq!(settings.rise_ms, DEFAULT_RISE_MS);
        assert_eq!(settings.fall_ms, DEFAULT_FALL_MS);
        assert_eq!(settings.slew_per_second, DEFAULT_SLEW_PER_SECOND);
        assert_eq!(lift.mode(), BlackLiftModeKind::Adaptive);
        let encoded = serde_json::to_value(&lift).unwrap();
        assert_eq!(encoded["darkThreshold"], 0.03);
        assert_eq!(encoded["riseMs"], DEFAULT_RISE_MS);
    }

    #[test]
    fn validation_rejects_bad_levels_thresholds_and_rates() {
        assert!(BlackLift::Number(-0.01).validate().is_err());
        assert!(BlackLift::Number(0.51).validate().is_err());
        let bad_thresholds = BlackLift::Tagged(BlackLiftMode::Adaptive {
            level: 0.1,
            dark_threshold: 0.4,
            bright_threshold: 0.4,
            rise_ms: 1_000.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        });
        assert!(bad_thresholds.validate().is_err());
        let bad_time = BlackLift::Tagged(BlackLiftMode::Adaptive {
            level: 0.1,
            dark_threshold: 0.02,
            bright_threshold: 0.2,
            rise_ms: 0.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        });
        assert!(bad_time.validate().is_err());
        let mut valid = bad_thresholds.adaptive().unwrap();
        assert!(valid.validate().is_err());
        valid.dark_threshold = f64::NAN;
        assert!(valid.validate().is_err());
        valid = AdaptiveBlackLift {
            level: 0.1,
            dark_threshold: 0.02,
            bright_threshold: 0.2,
            rise_ms: 1_000.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        };
        assert!(valid.validate().is_ok());
        valid.slew_per_second = 0.0;
        assert!(valid.validate().is_err());
    }
}
