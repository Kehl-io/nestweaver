use thiserror::Error;

/// Why an in-flight query was cancelled.
///
/// A cancelled computation is *incomplete*, not empty: it must propagate as a
/// distinct error and must never be treated as a legitimate result (or cached).
///
/// Both cancel triggers (query timeout and client disconnect) share a single
/// cooperative `AtomicBool`, which cannot carry a reason — so the leaf always
/// reports `Timeout`. On a client disconnect the request future is dropped
/// before any error is observed, so no distinct reason is needed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// The per-tool query deadline fired (or the shared flag was tripped).
    Timeout,
}

impl std::fmt::Display for CancelReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CancelReason::Timeout => write!(f, "timeout"),
        }
    }
}

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),
    #[error("query error: {0}")]
    Query(String),
    /// A ranking query was refused while an index publication is in flight:
    /// the ranking caches may describe a graph that no longer exists, so the
    /// query fails closed rather than answering with a successful-looking
    /// empty result (see the `ranking` module-header contract). Transient —
    /// callers should surface it as "temporarily unavailable", never as an
    /// empty graph.
    #[error("PageRank unavailable during dirty index publication")]
    RankingUnavailable,
    #[error("not found")]
    NotFound,
    #[error("presentation limit {limit} exceeds maximum {max}")]
    PresentationLimitExceeded { limit: usize, max: usize },
    /// The computation was cancelled cooperatively before it could finish.
    /// Distinct from an empty result so callers never cache a truncated answer.
    #[error("query cancelled: {0}")]
    Cancelled(CancelReason),
    /// A returned string value failed a corruption check (an embedded NUL —
    /// never valid in any column we store). The underlying storage engine can
    /// return garbled non-primary-key strings from partial scans after
    /// delete+checkpoint cycles (LadybugDB #678); surfacing this as a distinct
    /// LOUD error is what stops a corrupted value being returned silently.
    #[error("corrupt value at column {column}: {reason}")]
    CorruptValue { column: usize, reason: String },
    /// The mapped embedding artifact failed its payload checksum, so none of
    /// its vectors can be trusted.
    ///
    /// Surfaced as a distinct LOUD error for the same reason as
    /// [`StoreError::RankingUnavailable`]: a corrupt base must degrade
    /// semantic search VISIBLY. Returning an empty result instead is
    /// indistinguishable from "this corpus has no semantic matches", and the
    /// caller then reports `semantic_applied: true` over zero contribution —
    /// exactly the silent success the deferred-verification change (nw-184)
    /// introduced.
    #[error(
        "embedding artifact payload failed its checksum; semantic search is unavailable until a re-embed"
    )]
    EmbeddingArtifactCorrupt,
}

impl StoreError {
    pub fn is_duplicate(&self) -> bool {
        match self {
            StoreError::Database(msg) | StoreError::Query(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("already exist")
                    || lower.contains("duplicate")
                    || lower.contains("unique")
                    || lower.contains("constraint")
            }
            StoreError::NotFound
            | StoreError::RankingUnavailable
            | StoreError::PresentationLimitExceeded { .. }
            | StoreError::Cancelled(_)
            | StoreError::CorruptValue { .. }
            | StoreError::EmbeddingArtifactCorrupt => false,
        }
    }

    /// True when this error represents a cancelled (incomplete) computation.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, StoreError::Cancelled(_))
    }

    /// The cancellation reason, if this is a `Cancelled` error.
    pub fn cancel_reason(&self) -> Option<CancelReason> {
        match self {
            StoreError::Cancelled(reason) => Some(*reason),
            _ => None,
        }
    }
}

impl From<lbug::Error> for StoreError {
    fn from(e: lbug::Error) -> Self {
        StoreError::Database(e.to_string())
    }
}
