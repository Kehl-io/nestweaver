use std::path::{Path, PathBuf};

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

/// What KIND of corruption the storage engine reported.
///
/// nw-346. Before this existed, every corruption decision in the product was a
/// substring match on the engine's English prose: seven classifiers across
/// three crates, ~15 phrases, zero types. `From<lbug::Error>` collapsed every
/// engine failure into [`StoreError::Database`], so the one frame that
/// provably held an engine error threw that fact away and each consumer
/// re-derived it — from different substrings of different messages, which is
/// how one condition ended up with three mutually contradictory remedies
/// (nw-332).
///
/// The set is deliberately CLOSED and deliberately small. Each variant exists
/// because it wants a DIFFERENT remedy; a distinction that does not change the
/// advice does not belong here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionKind {
    /// The write-ahead log exists but its records cannot be read
    /// ("Corrupted wal file. Read out invalid WAL record type."). The database
    /// FILE is typically intact — this is the shape a five-artifact move-aside
    /// recovered in the nw-332 outage — so it must never inherit the
    /// "delete this database" remedy.
    WalUnreadable,
    /// The log is readable and simply has not been replayed, which a read-only
    /// open can never do. Recoverable by opening read-write.
    WalUnreplayed,
    /// The engine's catalog points past the end of the file. Nothing that runs
    /// later lengthens a truncated file.
    FileTruncated,
    /// The vendored C++ tripped one of its own invariants and unwound rather
    /// than faulting, so `open_crash_guard` never sees it.
    EngineAssertion,
    /// Corruption we could not name.
    ///
    /// LOAD-BEARING. It is what an unrecognised engine phrase becomes once
    /// context establishes corruption, so a wording change upstream degrades
    /// to "corruption, kind unknown" rather than silently reclassifying a
    /// corrupt database as a generic error with no remedy at all.
    Unclassified,
}

impl std::fmt::Display for CorruptionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            CorruptionKind::WalUnreadable => "unreadable write-ahead log",
            CorruptionKind::WalUnreplayed => "unreplayed write-ahead log",
            CorruptionKind::FileTruncated => "truncated database file",
            CorruptionKind::EngineAssertion => "storage engine assertion",
            CorruptionKind::Unclassified => "unclassified corruption",
        };
        f.write_str(name)
    }
}

/// Classify an engine message into a [`CorruptionKind`], or `None` when it
/// describes an ordinary failure.
///
/// This is THE classifier. Every phrase in it was moved here verbatim from a
/// call site — `into_diagnostic`'s `engine_corruption` block and its WAL arm in
/// `src/main.rs`, and `daemon_held_store_error` — so the change is a
/// RELOCATION, not new logic. Adding a phrase anywhere else re-opens nw-346.
///
/// Order matters: `WalUnreadable` is tested before `WalUnreplayed` because the
/// nw-332 message ("Corrupted wal file. Read out invalid WAL record type.") is
/// about a log that cannot be READ, and answering it with "open read-write to
/// replay" is how the crash-restart loop starts.
pub fn classify_engine_corruption(message: &str) -> Option<CorruptionKind> {
    let lower = message.to_lowercase();
    // The engine has at least TWO phrasings for an unreadable log and they do
    // not share a word order:
    //
    //   "Corrupted wal file. Read out invalid WAL record type."
    //   "Storage exception: Checksum verification failed, the WAL file is corrupted."
    //
    // The first came from the nw-332 outage. The SECOND was found by executing
    // the recovery runbook on a temp database — write garbage to `<db>.wal`
    // with no `<db>.shadow` beside it and every open reports it, no crash
    // required. A phrase list captured from one incident is a phrase list that
    // has seen one incident; this is the whole reason the classification lives
    // in one function with `Unclassified` underneath it.
    if lower.contains("corrupted wal") || (lower.contains("wal") && lower.contains("corrupt")) {
        return Some(CorruptionKind::WalUnreadable);
    }
    if lower.contains("shadow pages") || (lower.contains("replay") && lower.contains("read-only")) {
        return Some(CorruptionKind::WalUnreplayed);
    }
    if lower.contains("outside the database file") {
        return Some(CorruptionKind::FileTruncated);
    }
    if lower.contains("assertion failed in file") {
        return Some(CorruptionKind::EngineAssertion);
    }
    // A bare C++ exception `what()` with no sentence in it. Matched both as the
    // raw engine text and as the already-Displayed `StoreError` prose, because
    // this arm is reached from both directions.
    if lower.starts_with("basic_string") || lower.contains("database error: basic_string") {
        return Some(CorruptionKind::EngineAssertion);
    }
    None
}

/// The payload of [`StoreError::Corruption`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCorruption {
    pub kind: CorruptionKind,
    /// The engine's own words, VERBATIM.
    pub detail: String,
    /// The database this is about. Engine messages name no path, which is why
    /// the CLI kept a `LAST_OPENED_DB` global to guess one; a typed error can
    /// simply carry it. Filled in by the store's single open funnel, which is
    /// the frame that knows.
    pub path: Option<PathBuf>,
}

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),
    #[error("query error: {0}")]
    Query(String),
    /// nw-346. Corruption the STORAGE ENGINE reported, classified once at the
    /// FFI boundary — the only frame that provably knows an error came from the
    /// engine.
    ///
    /// The taxonomy already had two corruption variants before this one, and
    /// both describe corruption NestWeaver detected ITSELF: [`Self::CorruptValue`]
    /// (an embedded NUL in a returned string) and [`Self::EmbeddingArtifactCorrupt`]
    /// (a checksum failure). There was no variant for corruption the engine
    /// reported, which is the only kind that bears on opening a database at
    /// all. That asymmetry is nw-346 in one sentence.
    ///
    /// `detail` carries the engine's own words VERBATIM. It is the only thing
    /// that says why, and a paraphrase at this boundary is what let
    /// `daemon_held_store_error` destroy the evidence it had just classified on.
    ///
    /// BOXED deliberately. Inline, this variant is the largest in the enum and
    /// it grew `StoreError` past the point where `clippy::result_large_err`
    /// fires on `DeleteProjectCascadeError` three crates away — an error type
    /// paying, on every `Ok` return, for a payload only the rarest failure
    /// carries.
    #[error("database corruption ({}): {}", .0.kind, .0.detail)]
    Corruption(Box<EngineCorruption>),
    /// A ranking query was refused while an index publication is in flight:
    /// the ranking caches may describe a graph that no longer exists, so the
    /// query fails closed rather than answering with a successful-looking
    /// empty result (see the `ranking` module-header contract). Transient —
    /// callers should surface it as "temporarily unavailable", never as an
    /// empty graph.
    #[error("PageRank unavailable during dirty index publication")]
    RankingUnavailable,
    /// Persisted embedding metadata exists but cannot be parsed or validated.
    ///
    /// This must remain distinct from an ordinary query failure: semantic
    /// callers are allowed to tolerate model/network availability failures,
    /// but must never turn an unverified database identity into a successful
    /// lexical-only answer.
    #[error("embedding identity is unreadable: {detail}")]
    EmbeddingIdentityUnreadable { detail: String },
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
            StoreError::Corruption(_)
            | StoreError::EmbeddingIdentityUnreadable { .. }
            | StoreError::NotFound
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

    /// Build a `StoreError` from a raw storage-engine message, classifying
    /// corruption ONCE.
    ///
    /// This is what `From<lbug::Error>` calls. It is public so the classifier
    /// can be exercised on the engine's exact phrasings — captured
    /// character-for-character from real incidents — without needing the crash
    /// that produced them.
    pub fn from_engine_message(message: impl Into<String>) -> Self {
        let detail = message.into();
        match classify_engine_corruption(&detail) {
            Some(kind) => StoreError::Corruption(Box::new(EngineCorruption {
                kind,
                detail,
                path: None,
            })),
            None => StoreError::Database(detail),
        }
    }

    /// Record corruption whose engine phrasing we do not recognise.
    ///
    /// For callers that know from CONTEXT that a database is damaged even
    /// though the message is unfamiliar. The point of the variant is that such
    /// a case degrades to "corruption, kind unknown" instead of falling back to
    /// [`Self::Database`], which carries no remedy at all.
    pub fn corruption_unclassified(message: impl Into<String>) -> Self {
        StoreError::Corruption(Box::new(EngineCorruption {
            kind: CorruptionKind::Unclassified,
            detail: message.into(),
            path: None,
        }))
    }

    /// True when the engine refused the open because another process holds the
    /// write lock.
    ///
    /// nw-346. Lives here for the same reason `corruption_kind` does: `repair`
    /// and the CLI read funnel both need to tell "someone holds it" from "it is
    /// damaged", and they were each deciding that from a different substring of
    /// a different message. One phrase, one place.
    pub fn is_lock_contention(&self) -> bool {
        match self {
            StoreError::Database(msg) | StoreError::Query(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("could not set lock") || lower.contains("another process holds")
            }
            _ => false,
        }
    }

    /// The corruption kind, if this error is engine-reported corruption.
    ///
    /// Sits beside `is_duplicate` / `is_cancelled` so consumers match a variant
    /// rather than re-deriving prose one frame after it was structured.
    pub fn corruption_kind(&self) -> Option<CorruptionKind> {
        match self {
            StoreError::Corruption(corruption) => Some(corruption.kind),
            _ => None,
        }
    }

    /// The engine's verbatim words, for corruption errors.
    pub fn corruption_detail(&self) -> Option<&str> {
        match self {
            StoreError::Corruption(corruption) => Some(corruption.detail.as_str()),
            _ => None,
        }
    }

    /// The database this corruption is about, when the open funnel recorded it.
    pub fn corruption_path(&self) -> Option<&Path> {
        match self {
            StoreError::Corruption(corruption) => corruption.path.as_deref(),
            _ => None,
        }
    }

    /// Name the database a corruption error is about.
    ///
    /// Only fills an EMPTY slot: a path attached closer to the failure is
    /// better evidence than one attached by an outer frame, and a retry loop
    /// must not be able to relabel an error with a different database.
    #[must_use]
    pub fn with_db_path(self, db_path: &Path) -> Self {
        match self {
            StoreError::Corruption(mut corruption) if corruption.path.is_none() => {
                corruption.path = Some(db_path.to_path_buf());
                StoreError::Corruption(corruption)
            }
            other => other,
        }
    }
}

impl From<lbug::Error> for StoreError {
    fn from(e: lbug::Error) -> Self {
        // nw-346. Classify HERE — the single frame that provably holds an
        // engine error. Everything downstream matches a variant.
        StoreError::from_engine_message(e.to_string())
    }
}

#[cfg(test)]
mod corruption_classification_tests {
    use super::*;

    /// nw-346. The engine's five known corruption phrasings must each classify
    /// to a KIND, at the FFI boundary, exactly once. Verbatim strings, captured
    /// from real incidents (nw-285's three reproductions and nw-332's outage) —
    /// so a phrasing change fails HERE, in one file, rather than silently
    /// unselecting a recovery path three crates away.
    #[test]
    fn every_known_engine_corruption_phrase_classifies_to_a_kind() {
        for (phrase, expected) in [
            (
                "Corrupted wal file. Read out invalid WAL record type.",
                CorruptionKind::WalUnreadable,
            ),
            // Reproduced deterministically on a temp database (garbage `.wal`,
            // no `.shadow`), captured verbatim from the engine. The first
            // phrasing does not match it and neither did the classifier until
            // the runbook was actually executed.
            (
                "Storage exception: Checksum verification failed, the WAL file is corrupted.",
                CorruptionKind::WalUnreadable,
            ),
            (
                "catalog page range starts at 3567 and spans 5 pages, outside the \
                 database file with 1696 pages",
                CorruptionKind::FileTruncated,
            ),
            (
                "Assertion failed in file \"<crate>/column.cpp\" on line 289: \
                 startOffsetInSegment + length <= state.metadata.numValues",
                CorruptionKind::EngineAssertion,
            ),
            (
                "database error: basic_string",
                CorruptionKind::EngineAssertion,
            ),
            (
                "Couldn't replay shadow pages under read-only mode.",
                CorruptionKind::WalUnreplayed,
            ),
        ] {
            let error = StoreError::from_engine_message(phrase);
            assert_eq!(
                error.corruption_kind(),
                Some(expected),
                "engine phrase must classify at the FFI boundary, not at a call \
                 site: {phrase}"
            );
        }
    }

    /// The other direction, and the one that makes the first non-vacuous: an
    /// ordinary failure must NOT be called corruption.
    #[test]
    fn an_ordinary_engine_failure_is_not_corruption() {
        for benign in [
            "Could not set lock on file /tmp/x.lbug.lock",
            "Binder exception: Table Symbol does not exist.",
            "Runtime exception: Query interrupted.",
            "IO exception: Cannot open file /tmp/x.lbug.shadow: No such file or directory",
        ] {
            assert_eq!(
                StoreError::from_engine_message(benign).corruption_kind(),
                None,
                "a lock, a binder error, a cancellation and an orphaned log are \
                 not corruption: {benign}"
            );
        }
    }

    /// The drift guard, and the reason `Unclassified` exists. An engine phrase
    /// nobody has seen must still be reachable as corruption when the CONTEXT
    /// says so — it must never silently become `Database(String)` again.
    #[test]
    fn an_unrecognised_phrase_degrades_to_unclassified_not_to_generic() {
        let error = StoreError::corruption_unclassified("some future engine wording");
        assert!(matches!(
            error.corruption_kind(),
            Some(CorruptionKind::Unclassified)
        ));
        assert!(!matches!(error, StoreError::Database(_)));
    }

    /// The engine names no path, which is why the CLI kept a global to guess
    /// one. A typed error carries it — and only from the frame that knows,
    /// which is why a second attribution cannot overwrite the first.
    #[test]
    fn a_corruption_error_carries_the_database_it_is_about_and_keeps_the_first_one() {
        let error = StoreError::from_engine_message("Corrupted wal file.")
            .with_db_path(Path::new("/tmp/first.lbug"))
            .with_db_path(Path::new("/tmp/second.lbug"));
        assert_eq!(error.corruption_path(), Some(Path::new("/tmp/first.lbug")));

        // A non-corruption error is left exactly as it was.
        let benign = StoreError::from_engine_message("Could not set lock")
            .with_db_path(Path::new("/tmp/x.lbug"));
        assert!(matches!(benign, StoreError::Database(_)));
    }

    /// The engine's own words must survive classification verbatim. The nw-332
    /// regression was a call site that classified correctly and then paraphrased
    /// the evidence away, so the NEXT classifier downstream saw only the
    /// paraphrase and reached a different — contradictory — conclusion.
    #[test]
    fn classification_never_replaces_the_engines_own_words() {
        const ENGINE: &str = "Corrupted wal file. Read out invalid WAL record type.";
        let error = StoreError::from_engine_message(ENGINE);
        assert_eq!(error.corruption_detail(), Some(ENGINE));
        assert!(
            error.to_string().contains(ENGINE),
            "the rendered error must still carry the engine's sentence: {error}"
        );
    }
}
