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
pub mod probe;
pub mod reconciler;
pub mod snapshot;
pub mod state;
pub mod supervisor;
pub mod sway;
pub mod util;
pub mod wallpapers;

/// Version of the running daemon, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
