//! Suede — remote management daemon for Sway-based display appliances.
//!
//! The daemon is organised around one idea: clients write *desired state*, and a
//! reconciler continuously drives the live Sway session toward it. Observed
//! state is always re-derived from the compositor and never persisted.

pub mod api;
pub mod audio;
pub mod checks;
pub mod config;
pub mod error;
pub mod events;
pub mod model;
pub mod ports;
pub mod probe;
#[cfg(feature = "projection")]
pub mod projection;
pub mod reconciler;
pub mod snapshot;
pub mod state;
pub mod supervisor;
pub mod sway;
pub mod util;
pub mod wallpapers;

/// Version of the running daemon, from `Cargo.toml`.
///
/// The release this build is meant to be. Every build between two releases
/// carries the same value, so for "which build is this actually" see
/// [`BUILD_ID`].
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which build this is: `git describe` output, e.g. `v0.1.0-12-g81226ee`.
///
/// The last release, how far past it, and the exact commit. `unknown` when
/// built from a source tarball with neither git nor `SUEDE_BUILD_ID` set.
pub const BUILD_ID: &str = env!("SUEDE_BUILD_ID");

/// Version and build together, as reported to a person: `0.1.0 (v0.1.0-12-g81226ee)`.
///
/// Composed by the build script so it can be a `&'static str`, which is what
/// the command-line parser needs for `--version`. Falls back to the bare
/// version when there is no build to name.
pub const VERSION_STRING: &str = env!("SUEDE_VERSION_STRING");
