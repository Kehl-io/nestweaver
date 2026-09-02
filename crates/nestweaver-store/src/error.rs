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

/// The sentence the store attaches when it DECLINES to call a write-ahead log
/// corrupt because a live writer holds the database's write lease.
///
/// nw-404. Three jobs, which is why it is a `const` and not an inline
/// `format!`:
///
/// 1. It is the operator-facing disclosure. It says who to blame (another
///    process) and what to do (nothing — retry).
/// 2. It satisfies [`StoreError::is_lock_contention`] verbatim ("another
///    process holds"), so every consumer that already tells "someone holds it"
///    from "it is damaged" — `repair_open_failure`, `daemon_held_store_error` —
///    gets the right answer with no new predicate. nw-346 built that
///    distinction; this case simply never reached it.
/// 3. It is the SENTINEL [`classify_engine_corruption`] looks for. The engine's
///    verbatim words are preserved beside it (a paraphrase at this boundary is
///    the nw-332 regression), and those words still say "corrupted wal" — so
///    without the sentinel the prose-fallback classifier in `into_diagnostic`
///    (`src/main.rs`, which re-classifies the RENDERED error when no type
///    survived) would re-derive `WalUnreadable` from our own disclosure and
///    print the move-aside runbook anyway. Evidence and veto have to travel
///    together.
pub const LIVE_WRITER_DISCLOSURE: &str = "another process holds the write lease";

/// Path of the write-lease file for `db_path` — `<db>.write.lock`.
///
/// nw-404. DERIVED, not owned: the lease itself is
/// `nestweaver_daemon::lifecycle::acquire_db_write_lease`, and this crate sits
/// far below the daemon so it cannot call it. This function only NAMES the file
/// that mechanism uses; it never creates it and never locks it for keeps.
/// Keeping the two in sync is a real hazard — see the HANDOFF note on
/// [`live_writer_holds_write_lease`].
///
/// The canonicalisation mirrors `lifecycle::canonical_db_path` in the shape that
/// matters here (resolve the path, else resolve the parent and re-join the file
/// name). The lease's own cwd-join fallback is deliberately NOT reproduced: the
/// probe below tries the un-canonicalised name too, so a path this cannot
/// resolve costs an extra `open` on the error path rather than a wrong answer.
fn write_lease_path(db_path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(db_path).ok().or_else(|| {
        let parent = db_path.parent()?;
        let file_name = db_path.file_name()?;
        Some(std::fs::canonicalize(parent).ok()?.join(file_name))
    });
    let mut name = canonical
        .unwrap_or_else(|| db_path.to_path_buf())
        .into_os_string();
    name.push(".write.lock");
    PathBuf::from(name)
}

/// Is `lease_path` locked RIGHT NOW by a process that is still alive?
///
/// nw-404. The lease is an `flock`, and `flock` is the reason this is a proof
/// rather than an inference: the kernel drops it when the holder exits, so
/// there is no stale state to reap and no pid to check for liveness — a held
/// lock IS a live holder. That is the same property
/// `nestweaver_engine::index_publication::process_is_alive` reaches for by
/// checking `kill(pid, 0)` against the publication marker's recorded pid, with
/// the pid-reuse caveat removed because the kernel is doing the bookkeeping.
///
/// A SHARED probe, and it never creates the file. Both matter:
///
/// * `LOCK_SH` still conflicts with the holder's `LOCK_EX`, so `EWOULDBLOCK`
///   proves a writer holds it. Taking `LOCK_EX` to ask the same question would
///   make a concurrent `acquire_db_write_lease` fail with `Held` for the
///   microseconds we owned it — a probe that manufactures the contention it is
///   asking about.
/// * `create(false)`: an absent lease file is a real answer (nobody has ever
///   taken a lease on this database), and creating one would leave debris in
///   every directory a read-only open ever failed in.
///
/// Only reached on the corruption branch of a failed open, so the shared lock
/// is held for microseconds on a path that already ended in an error.
#[cfg(unix)]
fn write_lease_is_held(lease_path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;

    let Ok(file) = std::fs::OpenOptions::new().read(true).open(lease_path) else {
        // No lease file, or we cannot read it. Either way we have not PROVED a
        // live writer, and the counterweight says an unproved writer must not
        // suppress a genuine corruption report.
        return false;
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        // EWOULDBLOCK is the answer we came for. Anything else (EBADF, EINTR,
        // an unsupported filesystem) is "cannot tell", which is not proof.
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    }
    // We got the shared lock, so nobody held it exclusively. Release it at once
    // rather than waiting for the drop, so the window cannot outlive the answer.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    false
}

/// Non-unix fallback: there is no portable equivalent of the `flock` probe, so
/// we cannot PROVE a live writer and therefore never claim one.
///
/// Note which direction this fails in, because it is the opposite of
/// `process_is_alive`'s: that one returns `true` on non-unix so recovery
/// DECLINES. Here `true` would suppress every WAL-corruption report on the
/// platform, breaking the counterweight everywhere rather than in the one
/// contended case. `false` preserves the pre-nw-404 behaviour exactly.
#[cfg(not(unix))]
fn write_lease_is_held(_lease_path: &Path) -> bool {
    false
}

/// Is a live writer holding the write lease on `db_path` right now?
///
/// nw-404, the primitive the classifier consults before it is willing to call a
/// write-ahead log damaged.
///
/// HANDOFF (out of this crate's reach): `write_lease_path` here and
/// `nestweaver_daemon::lifecycle::write_lease_path` derive the same
/// `<db>.write.lock` name independently, because the store cannot depend on the
/// daemon. The lasting shape is for the daemon's to delegate to this one; that
/// is a `crates/nestweaver-daemon/src/lifecycle.rs` edit. Until then the probe
/// asks about BOTH the canonicalised and the literal name — a lease held under
/// either is a live writer, and asking twice on an error path is free.
pub fn live_writer_holds_write_lease(db_path: &Path) -> bool {
    let canonical = write_lease_path(db_path);
    if write_lease_is_held(&canonical) {
        return true;
    }
    let mut literal = db_path.to_path_buf().into_os_string();
    literal.push(".write.lock");
    let literal = PathBuf::from(literal);
    literal != canonical && write_lease_is_held(&literal)
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
///
/// PURE, and deliberately so: it decides from the MESSAGE alone, which is what
/// lets `into_diagnostic` re-run it over rendered prose when no type survived.
/// The writer-liveness question needs a database path, so it lives in
/// [`classify_engine_corruption_for_db`] — the variant the store's open funnel
/// calls, where a path is in hand.
pub fn classify_engine_corruption(message: &str) -> Option<CorruptionKind> {
    let lower = message.to_lowercase();
    // nw-404. The one exception to "decide from the message alone", and it is
    // not a heuristic: this phrase is only ever written by
    // `StoreError::from_engine_message_for_db` AFTER it proved a live writer
    // holds the lease. The engine's verbatim words ride along in the same
    // string and still say "corrupted wal", so without this the prose fallback
    // in `into_diagnostic` re-derives `WalUnreadable` from our own disclosure
    // and prints the move-aside-your-WAL runbook against a healthy database
    // being written by the running daemon. A verdict must not be overturned by
    // the evidence it was careful to preserve.
    if lower.contains(LIVE_WRITER_DISCLOSURE) {
        return None;
    }
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

/// [`classify_engine_corruption`] for a caller that knows WHICH database
/// failed, with the writer-liveness veto nw-404 exists for.
///
/// THE BUG: a read-only open that meets a live writer's partially-appended WAL
/// tail gets "Corrupted wal file. Read out invalid WAL record type." from the
/// engine — the same sentence a genuinely damaged log produces. The pure
/// classifier cannot tell them apart, so it called a HEALTHY database corrupt
/// and the CLI printed a runbook telling the operator to move aside `.wal`,
/// `.wal.checkpoint`, `.shadow` and both `.checkpoint.*.lock` of a database the
/// daemon was actively appending to. Following it discards the un-checkpointed
/// tail. Measured on the live graph: `brain status` and `search` succeeded
/// throughout and the `.wal` GREW 12,226,075 -> 12,817,039 bytes in 20 seconds
/// while the direct route reported it corrupt.
///
/// THE VETO IS NARROW ON PURPOSE, and each narrowing is load-bearing:
///
/// * `WalUnreadable` ONLY. A truncated file, a C++ assertion or an
///   unclassified corruption is not made healthy by a writer being alive, and
///   suppressing those would trade a false alarm for a silent one.
/// * A PROVED live holder only. An absent lease file, a lease file nobody
///   holds, and a platform where we cannot ask all mean "no proof", and all
///   three keep the corruption verdict. A genuinely corrupt WAL with no live
///   writer must still be reported as corrupt — that is the counterweight, and
///   it is what stops this fix from becoming a way to never report corruption.
/// * The engine's own words survive into whatever the caller returns. The veto
///   changes the VERDICT, never the evidence.
/// * CALLER-SCOPED. `flock` carries no holder identity, so this cannot tell "someone
///   else is writing" from "I am writing" — and the daemon takes the lease before it
///   opens the store. The store's open funnel therefore calls this for READ-ONLY
///   opens only; `db::open_failure` carries that reasoning and the handoff that
///   would make the answer exact instead of structural. Anyone else calling this
///   owes the same question.
pub fn classify_engine_corruption_for_db(message: &str, db_path: &Path) -> Option<CorruptionKind> {
    let kind = classify_engine_corruption(message)?;
    if kind == CorruptionKind::WalUnreadable && live_writer_holds_write_lease(db_path) {
        return None;
    }
    Some(kind)
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

    /// [`Self::from_engine_message`] for the frame that knows WHICH database
    /// failed — the store's open funnel.
    ///
    /// nw-404. Two things the path-free constructor cannot do:
    ///
    /// 1. It consults [`classify_engine_corruption_for_db`], so an unreadable
    ///    WAL under a LIVE WRITER is reported as contention rather than damage.
    /// 2. It attaches the path at construction instead of leaving an outer
    ///    frame to add it, which is the nw-346 property `with_db_path` protects.
    ///
    /// The contention error is a plain [`Self::Database`] on purpose: it is the
    /// variant [`Self::is_lock_contention`] already answers for, so no consumer
    /// needs a new arm, and no new `CorruptionKind` (which would have to be a
    /// corruption, which is the claim being retracted). The engine's verbatim
    /// sentence is carried through — see [`LIVE_WRITER_DISCLOSURE`] for why
    /// carrying it is safe only with the sentinel in front of it.
    pub fn from_engine_message_for_db(message: impl Into<String>, db_path: &Path) -> Self {
        let detail = message.into();
        match classify_engine_corruption_for_db(&detail, db_path) {
            Some(kind) => StoreError::Corruption(Box::new(EngineCorruption {
                kind,
                detail,
                path: Some(db_path.to_path_buf()),
            })),
            None if classify_engine_corruption(&detail) == Some(CorruptionKind::WalUnreadable) => {
                StoreError::Database(format!(
                    "{LIVE_WRITER_DISCLOSURE} on {} and is appending to its \
                     write-ahead log right now, so the partial record at the end \
                     of the log is IN-FLIGHT WRITING, not damage — this database \
                     is not known to be corrupt and must not be recovered as if \
                     it were. Retry the read, or route it through the daemon that \
                     owns the write lock. The storage engine's own words, kept \
                     verbatim: {detail}",
                    db_path.display()
                ))
            }
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
    ///
    /// nw-404 added a THIRD producer and no new phrase:
    /// [`LIVE_WRITER_DISCLOSURE`] contains "another process holds" verbatim, so
    /// a WAL that is merely being appended to lands in the arm this predicate
    /// already owned. That case used to fall straight through here into
    /// corruption, which is the whole defect — the distinction existed and this
    /// condition was never routed into it.
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

    /// nw-404. The engine says the same sentence for a log that is DAMAGED and
    /// for a log that is being APPENDED TO, so a lease held by a live writer is
    /// the only thing that separates them.
    ///
    /// `flock` is per-open-file-description, so a second `open` in this very
    /// process contends with the first exactly as a second process would —
    /// which is what lets the holder be forked-free here.
    #[cfg(unix)]
    struct HeldLease {
        /// Held for the lifetime of the value; the kernel releases it on drop,
        /// which is exactly the property the probe is asking about.
        _file: std::fs::File,
    }

    #[cfg(unix)]
    impl HeldLease {
        /// Take the lease the way `acquire_db_write_lease` does: create
        /// `<db>.write.lock`, `flock(LOCK_EX | LOCK_NB)`, hold the descriptor.
        fn take(db_path: &Path) -> Self {
            use std::os::unix::io::AsRawFd;
            let mut name = db_path.to_path_buf().into_os_string();
            name.push(".write.lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(PathBuf::from(name))
                .expect("lease file");
            assert_eq!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0,
                "the test must actually hold the lease or it proves nothing"
            );
            HeldLease { _file: file }
        }
    }

    /// The exact sentence the live kory-brain graph produced while the daemon
    /// (pid 45771) was appending to a `.wal` that GREW 12,226,075 -> 12,817,039
    /// bytes in 20 seconds and `brain status` / `search` kept succeeding.
    const LIVE_WRITER_WAL_PHRASE: &str = "Corrupted wal file. Read out invalid WAL record type.";

    /// nw-404. A read-only open that meets a live writer must report
    /// CONTENTION, not corruption — and the runbook that tells the operator to
    /// move aside a live database's `.wal` must be unreachable from it.
    ///
    /// The third assertion is the one that actually stops the data loss:
    /// `into_diagnostic` re-runs the PURE classifier over the RENDERED error
    /// when no type survived, so an error that carries the engine's verbatim
    /// "corrupted wal" would be re-promoted to `WalUnreadable` there and print
    /// the runbook after all. Retracting the verdict is not enough; the
    /// retraction has to survive re-classification.
    #[cfg(unix)]
    #[test]
    fn a_wal_a_live_writer_is_appending_is_contention_and_can_never_reach_the_runbook() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"").unwrap();
        let _lease = HeldLease::take(&db);

        assert!(
            live_writer_holds_write_lease(&db),
            "a held flock on <db>.write.lock is the writer signal"
        );
        assert_eq!(
            classify_engine_corruption_for_db(LIVE_WRITER_WAL_PHRASE, &db),
            None,
            "a healthy database being written must not be classified as damaged"
        );

        let error = StoreError::from_engine_message_for_db(LIVE_WRITER_WAL_PHRASE, &db);
        assert_eq!(
            error.corruption_kind(),
            None,
            "no corruption kind means no `corruption_diagnostic`, which means no runbook"
        );
        assert!(
            error.is_lock_contention(),
            "it must land in the existing 'someone holds it' arm nw-346 built: {error}"
        );
        assert!(
            error.to_string().contains(LIVE_WRITER_WAL_PHRASE),
            "the engine's own words must survive the retraction: {error}"
        );
        assert_eq!(
            classify_engine_corruption(&format!("{error}")),
            None,
            "the prose fallback in `into_diagnostic` must not re-promote our own \
             disclosure back to WalUnreadable and print the move-aside runbook"
        );
    }

    /// THE COUNTERWEIGHT. The veto must be reachable ONLY by proof of a live
    /// holder, or nw-404's fix becomes a way to never report a corrupt WAL at
    /// all — which is the failure mode with the worse ending.
    ///
    /// Three "no proof" shapes, all of which must still report corruption:
    /// no lease file at all, a lease file nobody holds (it is never deleted
    /// after a release, so presence alone is evidence of nothing), and a lease
    /// that WAS held and has since been dropped.
    #[cfg(unix)]
    #[test]
    fn a_corrupt_wal_with_no_live_writer_is_still_reported_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"").unwrap();

        let expect_corrupt = |stage: &str| {
            assert!(
                !live_writer_holds_write_lease(&db),
                "no live writer must be claimed at stage: {stage}"
            );
            assert_eq!(
                classify_engine_corruption_for_db(LIVE_WRITER_WAL_PHRASE, &db),
                Some(CorruptionKind::WalUnreadable),
                "a genuinely unreadable log must keep its verdict at stage: {stage}"
            );
            assert_eq!(
                StoreError::from_engine_message_for_db(LIVE_WRITER_WAL_PHRASE, &db)
                    .corruption_kind(),
                Some(CorruptionKind::WalUnreadable),
                "and the operator must still get the recovery runbook at stage: {stage}"
            );
        };

        expect_corrupt("no lease file has ever existed");

        // A lease file with nobody holding it. `acquire_db_write_lease` never
        // deletes it, so every database a writer has EVER touched has one.
        let mut name = db.clone().into_os_string();
        name.push(".write.lock");
        std::fs::write(PathBuf::from(name), b"").unwrap();
        expect_corrupt("the lease file exists but is unheld");

        let lease = HeldLease::take(&db);
        assert!(live_writer_holds_write_lease(&db));
        drop(lease);
        expect_corrupt("the lease was held and has been released");
    }

    /// The other half of "keep it narrow": a live writer does not make a
    /// TRUNCATED file, a C++ assertion or an unclassified corruption healthy.
    /// Only the WAL-tail reading is ambiguous under an active append, so only
    /// that verdict may be retracted.
    #[cfg(unix)]
    #[test]
    fn a_live_writer_does_not_excuse_any_corruption_but_the_wal_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"").unwrap();
        let _lease = HeldLease::take(&db);

        for (phrase, expected) in [
            (
                "catalog page range starts at 3567 and spans 5 pages, outside the \
                 database file with 1696 pages",
                CorruptionKind::FileTruncated,
            ),
            (
                "Assertion failed in file \"<crate>/column.cpp\" on line 289: x <= y",
                CorruptionKind::EngineAssertion,
            ),
            (
                "database error: basic_string",
                CorruptionKind::EngineAssertion,
            ),
        ] {
            assert_eq!(
                classify_engine_corruption_for_db(phrase, &db),
                Some(expected),
                "a live writer must not suppress this verdict: {phrase}"
            );
        }

        // And an ordinary failure is still not corruption, lease or no lease.
        assert_eq!(
            classify_engine_corruption_for_db(
                "Binder exception: Table Symbol does not exist.",
                &db
            ),
            None
        );
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
