//! CLI command handlers and helpers split out of `src/main.rs` (nw-348).
//!
//! Every item here is re-exported into the crate root (`use cli::*::*` in
//! `main.rs`), so call sites and the `use super::*` test modules in `main.rs`
//! resolve them exactly as before the move.

pub(crate) mod backup;
pub(crate) mod brain;
pub(crate) mod brain_render;
pub(crate) mod contracts;
pub(crate) mod daemon_rpc;
pub(crate) mod embed;
pub(crate) mod eval;
pub(crate) mod instance;
pub(crate) mod memory;
