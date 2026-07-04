//! Federation coordinator — the host-agnostic half of NestWeaver's hybrid
//! (local + upstream) query engine.
//!
//! This crate holds everything the hybrid client needs that does NOT touch a
//! local `DaemonClient`: upstream discovery and configuration, the per-tool
//! routing matrix, upstream handles with health/ejection/latency state,
//! result merging (RRF + scope-hash dedup), provenance injection, two-tier
//! impact composition, cross-repo trace boundary detection/stitching, and the
//! JSON-RPC dispatch helpers that speak to any NestWeaver gRPC endpoint.
//!
//! The LOCAL tier is always parameterized: callers (today `nestweaver-client`,
//! later the daemon-side coordinator) compute local results themselves and
//! feed them into the shared functions here — e.g. [`two_tier::two_tier_query`]
//! takes the already-computed local result, and
//! [`health::compute_stale_repos`] takes the local repo states as data.

pub mod dedup;
pub mod discovery;
pub mod dispatch;
pub mod health;
pub mod merge;
pub mod repo_identity;
pub mod results;
pub mod routing;
pub mod trace;
pub mod two_tier;
pub mod upstream;
