use thiserror::Error;

/// Why an in-flight query was cancelled.
///
/// A cancelled computation is *incomplete*, not empty: it must propagate as a
/// distinct error and must never be treated as a legitimate result (or cached).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// The per-tool query deadline fired.
    Timeout,
    /// The request future was dropped (client cancel / disconnect).
    ClientDisconnected,
}

impl std::fmt::Display for CancelReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CancelReason::Timeout => write!(f, "timeout"),
            CancelReason::ClientDisconnected => write!(f, "client disconnected"),
        }
    }
}

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("not found")]
    NotFound,
    /// The computation was cancelled cooperatively before it could finish.
    /// Distinct from an empty result so callers never cache a truncated answer.
    #[error("query cancelled: {0}")]
    Cancelled(CancelReason),
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
            StoreError::NotFound | StoreError::Cancelled(_) => false,
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
