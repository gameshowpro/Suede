//! Domain types.
//!
//! [`observed`] holds what Sway and PipeWire report; [`desired`] holds what the
//! client asked for. The two are never conflated.

pub mod desired;
pub mod observed;

pub use desired::*;
pub use observed::*;
