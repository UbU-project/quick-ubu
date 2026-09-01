//! `quick-ubu` core: v1 data types, the in-memory `Store`, and the deterministic
//! precompute functions. No I/O, no networking.

pub mod store;
pub mod types;

pub use store::Store;
pub use types::*;
