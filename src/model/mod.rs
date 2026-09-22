//! Domain types.
//!
//! [`observed`] holds what Sway and PipeWire report; [`desired`] holds what the
//! client asked for. The two are never conflated.

pub mod arrangement;
pub mod black_lift;
pub mod desired;
pub mod geometry;
pub mod limits;
pub mod observed;

pub use arrangement::*;
pub use black_lift::*;
pub use desired::*;
pub use geometry::*;
pub use observed::*;
