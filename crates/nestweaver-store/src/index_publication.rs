//! The `<db>.index-dirty` publication marker: its path, its payload, and how
//! to read that payload back.
//!
//! The marker is written durably by the indexing writer (pid + establishment
//! timestamp, `sync_all`, parent directory fsynced) precisely so it survives
//! process death. While it exists, canonical `.generation` and `.pagerank.json`
//! sidecars may predate the committed graph, so ranked queries fail closed.
//!
//! Until nw-C1 the payload was written and never read back. This module is the
//! read side: it turns the marker into a three-state answer — absent, present
//! (with whatever the payload says), or *undeterminable*.
//!
//! **The third state is load-bearing.** [`GraphStore::is_index_publication_dirty`]
//! is `try_exists().unwrap_or(true)`, so an `EACCES`/`EIO` on the sidecar
//! directory deliberately reads as permanently dirty. "Cannot tell" is not
//! "abandoned": recovery must never clear a marker it could not read.
//!
//! Liveness of `writer_pid` is intentionally NOT decided here — this crate has
//! no `libc` dependency. See `nestweaver_engine::index_publication` for the
//! liveness-aware view built on top of this.
//!
//! [`GraphStore::is_index_publication_dirty`]: crate::db::GraphStore::is_index_publication_dirty

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Marker payload reason recorded by a run that committed AFTER cancellation
/// was requested and therefore left its publication dirty **deliberately**.
///
/// See `nestweaver_engine::index::finalize_committed_index_for_scope_with_io`
/// (`publish_clean: false`). Recovery still reconciles such a publication once
/// its writer is dead — the sidecars really do predate the commit either way —
/// but it reports the distinction so the "the graph may be incomplete, run
/// `index --force`" guidance is not silently lost.
pub const MARKER_REASON_CANCELLED: &str = "cancelled";

/// Path of the durable publication marker for `db_path`.
///
/// Kept as a free function so callers that only have a path (the `repair`
/// command, `brain_status` on the direct `--no-daemon` path) do not need an
/// open store to name it.
pub fn marker_path(db_path: &Path) -> PathBuf {
    let mut value = db_path.as_os_str().to_owned();
    value.push(".index-dirty");
    PathBuf::from(value)
}

/// What the marker payload records about the writer that established it.
///
/// Every field is optional: a marker written by an older binary, truncated by a
/// crash between `create` and `write_all`, or hand-created by an operator (the
/// pre-nw-C1 `touch`/`echo` escape hatch) still counts as *present*. A marker
/// we cannot attribute is never treated as abandoned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerRecord {
    /// The pid recorded by the establishing writer, when the payload parsed.
    pub writer_pid: Option<i32>,
    /// Wall-clock establishment time, when the payload parsed.
    pub established_unix_nanos: Option<u128>,
    /// Optional reason field (see [`MARKER_REASON_CANCELLED`]). Absent on the
    /// ordinary `{pid}:{nanos}` payload every writer has always written.
    pub reason: Option<String>,
}

impl MarkerRecord {
    /// How long ago the marker was established, per its own payload.
    ///
    /// `None` when the payload carried no timestamp, or when the recorded
    /// timestamp is in the future (a clock step backwards) — an age we cannot
    /// compute must not read as "old", because "old" is one of the two
    /// conditions that classify a publication as wedged.
    pub fn age(&self) -> Option<Duration> {
        let established = self.established_unix_nanos?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let delta = now.checked_sub(established)?;
        u64::try_from(delta).ok().map(Duration::from_nanos)
    }

    /// True when this publication was left dirty on purpose by a
    /// committed-after-cancellation run.
    pub fn is_deliberately_dirty(&self) -> bool {
        self.reason.as_deref() == Some(MARKER_REASON_CANCELLED)
    }
}

/// Three-state view of the marker. See the module docs on why "undeterminable"
/// is distinct from "present".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker: publication is clean.
    Absent,
    /// A marker exists. Payload fields are best-effort.
    Present(MarkerRecord),
    /// The marker's state could not be determined (permissions, I/O error, or
    /// a directory where a file belongs). Fail closed; never recover.
    Undeterminable(String),
}

impl MarkerState {
    /// Whether ranked queries must fail closed. Matches
    /// [`GraphStore::is_index_publication_dirty`]'s `unwrap_or(true)` exactly.
    ///
    /// [`GraphStore::is_index_publication_dirty`]: crate::db::GraphStore::is_index_publication_dirty
    pub fn is_dirty(&self) -> bool {
        !matches!(self, MarkerState::Absent)
    }

    /// The parsed payload, when the marker is present and readable.
    pub fn record(&self) -> Option<&MarkerRecord> {
        match self {
            MarkerState::Present(record) => Some(record),
            _ => None,
        }
    }
}

/// Serialize a marker payload. The `{pid}:{nanos}` prefix is byte-identical to
/// what every prior release wrote; `reason` appends a third field that older
/// readers (which never read the payload at all) cannot be confused by.
pub fn format_marker_payload(pid: u32, unix_nanos: u128, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("{pid}:{unix_nanos}:{reason}\n"),
        None => format!("{pid}:{unix_nanos}\n"),
    }
}

/// Parse a marker payload. Never fails: an unrecognised payload yields a
/// [`MarkerRecord`] with no attribution, which callers treat as
/// "present but unattributable" — dirty, but not abandoned.
pub fn parse_marker_payload(contents: &str) -> MarkerRecord {
    let trimmed = contents.trim();
    let mut fields = trimmed.split(':');
    let writer_pid = fields
        .next()
        .and_then(|f| f.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0);
    let established_unix_nanos = fields.next().and_then(|f| f.trim().parse::<u128>().ok());
    let reason = fields
        .next()
        .map(|f| f.trim().to_string())
        .filter(|r| !r.is_empty());
    MarkerRecord {
        writer_pid,
        established_unix_nanos,
        reason,
    }
}

/// Read the marker for `db_path`.
///
/// A `NotFound` is [`MarkerState::Absent`]. Any other I/O error — including the
/// `EISDIR` produced by the `unreadable_index_publication_marker_*` tests, which
/// create a *directory* at the marker path — is
/// [`MarkerState::Undeterminable`], never `Absent`.
pub fn read_marker(db_path: &Path) -> MarkerState {
    read_marker_at(&marker_path(db_path))
}

/// [`read_marker`] against an already-resolved marker path.
pub fn read_marker_at(path: &Path) -> MarkerState {
    match std::fs::read_to_string(path) {
        Ok(contents) => MarkerState::Present(parse_marker_payload(&contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // `read_to_string` reports NotFound only for a genuinely absent
            // path. Re-confirm through `try_exists` so a racing establish
            // between the two calls fails closed rather than open.
            match path.try_exists() {
                Ok(false) => MarkerState::Absent,
                Ok(true) => MarkerState::Present(MarkerRecord {
                    writer_pid: None,
                    established_unix_nanos: None,
                    reason: None,
                }),
                Err(error) => MarkerState::Undeterminable(error.to_string()),
            }
        }
        Err(error) => MarkerState::Undeterminable(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_legacy_pid_and_nanos_payload() {
        let record = parse_marker_payload("4242:1755000000000000000\n");
        assert_eq!(record.writer_pid, Some(4242));
        assert_eq!(
            record.established_unix_nanos,
            Some(1_755_000_000_000_000_000)
        );
        assert_eq!(record.reason, None);
        assert!(!record.is_deliberately_dirty());
    }

    #[test]
    fn parses_the_cancelled_reason_field() {
        let record = parse_marker_payload(&format_marker_payload(
            7,
            1_755_000_000_000_000_000,
            Some(MARKER_REASON_CANCELLED),
        ));
        assert_eq!(record.writer_pid, Some(7));
        assert!(record.is_deliberately_dirty());
    }

    #[test]
    fn an_unparseable_payload_is_present_but_unattributed() {
        let record = parse_marker_payload("dirty");
        assert_eq!(record.writer_pid, None);
        assert_eq!(record.established_unix_nanos, None);
        assert_eq!(record.age(), None);
    }

    #[test]
    fn a_nonpositive_pid_is_not_attribution() {
        assert_eq!(parse_marker_payload("0:1").writer_pid, None);
        assert_eq!(parse_marker_payload("-1:1").writer_pid, None);
    }

    #[test]
    fn absent_marker_reads_absent_and_not_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let state = read_marker(&dir.path().join("test.lbug"));
        assert_eq!(state, MarkerState::Absent);
        assert!(!state.is_dirty());
    }

    #[test]
    fn a_directory_at_the_marker_path_is_undeterminable_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        std::fs::create_dir(marker_path(&db_path)).unwrap();
        let state = read_marker(&db_path);
        assert!(
            matches!(state, MarkerState::Undeterminable(_)),
            "an unreadable marker must never read as absent: {state:?}"
        );
        assert!(state.is_dirty(), "undeterminable must still fail closed");
        assert!(state.record().is_none());
    }

    #[test]
    fn a_future_timestamp_yields_no_age_rather_than_a_stale_one() {
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            + 60_000_000_000;
        let record = parse_marker_payload(&format_marker_payload(1, future, None));
        assert_eq!(
            record.age(),
            None,
            "a clock step backwards must not make a young marker look wedged"
        );
    }
}
