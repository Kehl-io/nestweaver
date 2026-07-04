//! Normalized repo identity, shared across the federation coordinator and
//! result deduplication.
//!
//! The canonical implementation lives in [`nestweaver_schema::repo_url`] so
//! that UID minting ([`nestweaver_schema::repo_uid`],
//! [`nestweaver_schema::canonical_symbol_id`]) and coordinator-side result
//! deduplication key repo identity through the SAME normalizer — a single
//! source of truth. This module re-exports it for the merge and health
//! layers.
//!
//! Two NestWeaver instances (a LOCAL daemon and a SERVER) routinely index the
//! *same* repository under different clone-URL *forms* — an ssh remote
//! (`git@github.com:acme/api.git`) on one and the canonical https URL
//! (`https://github.com/acme/api`) on the other. Because both the minted UIDs
//! and the dedup keys funnel through [`normalized_repo_key`], those forms now
//! reconcile at the root.

pub use nestweaver_schema::repo_url::{normalized_repo_key, repo_name};
