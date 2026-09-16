#[cfg(feature = "acme")]
pub mod acme;
pub mod auth;
// nw-484: main-thread reload channel, off-write-gate artifact seeding, and
// bounded background auto-repair for the local embedding model cache.
// `pub(crate)` only — `server` is its sole external consumer (the
// `embed`/`plan_embed` RPC handlers and `run_server`).
mod embedding_repair;
#[cfg(target_os = "macos")]
pub mod launchd;
pub mod lifecycle;
pub mod safeguards;
pub mod server;
pub mod webhook;

pub use lifecycle::*;
pub use safeguards::*;
pub use server::*;
