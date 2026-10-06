//! Cargo-owned compiler recipes, strict decoding and explicit native bindings.
//!
//! This crate does not schedule processes, read ambient state, or admit a Nix
//! backend. Runtime owners supply the live jobserver and execution environment.
pub mod graph;
mod recipe;

pub use recipe::inspect;
pub use recipe::native::{BoundCommand, BoundGraph, BoundOutput, bind};
