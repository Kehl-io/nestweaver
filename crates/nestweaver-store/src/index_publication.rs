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

/// Marker payload reason recorded by a brain-watcher batch (nw-475, Task
/// 5.2, owner decision Q7), as opposed to a full `index` run.
///
/// A watcher batch's publication window covers a debounced set of short
/// per-file critical sections, not one atomic run — the graph is still
/// genuinely being published, but ranked reads may answer through it WITH
/// DISCLOSURE (`publication_in_progress`, `marker_age_s`, the in-flight note
/// paths) instead of failing closed, unlike a full `index` window. See
/// `GraphStore::index_publication_blocks_ranking` (nestweaver-store) and
/// `nestweaver_mcp::tools::dispatch_cancellable`.
pub const MARKER_REASON_WATCHER_BATCH: &str = "brain watcher batch";

/// How long a watcher-batch marker may keep the Q7 ranking exception.
///
/// The exception is for a debounce window (normally well under a second).
/// A leftover `brain watcher batch` reason while a multi-hour `index_repo`
/// holds the write lease is not that window — live kory-brain sat at
/// ~5.8h, `write_holder=index_repo`, `wedged=false`. After this age,
/// ranking fail-closes even when the write lease is still held. Unknown
/// age is fail-closed too: "cannot tell" is not "still a debounce".
pub const WATCHER_BATCH_EXCEPTION_MAX_AGE: Duration = Duration::from_secs(60);

/// Marker payload reason recorded by a code-link reconcile publication
/// (nw-670 live eval #2): one short chunk rewriting only derived note->code
/// links (REFERENCES_CODE) or project repo membership — never code or note
/// structure. Like a watcher batch, ranked reads answer through it with
/// disclosure instead of failing closed; a rules migration's relink is many
/// such chunks over minutes, and every read failed closed through it.
pub const MARKER_REASON_CODE_LINKS: &str = "code link reconciliation";

/// Whether `reason` names a publication ranked reads may answer through
/// with disclosure (a young one, with the writer lease held): a watcher
/// batch or a code-link reconcile chunk. Everything else fails closed.
pub fn is_serve_with_disclosure_reason(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some(MARKER_REASON_WATCHER_BATCH) | Some(MARKER_REASON_CODE_LINKS)
    )
}

/// True when this record is a *young* watcher-batch or code-link publication
/// that ranked reads may answer through (lease liveness is checked by the
/// caller).
pub fn watcher_batch_ranking_exception_applies(record: &MarkerRecord) -> bool {
    if !is_serve_with_disclosure_reason(record.reason.as_deref()) {
        return false;
    }
    matches!(record.age(), Some(age) if age <= WATCHER_BATCH_EXCEPTION_MAX_AGE)
}

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
    /// Bounded, in-flight note paths the establishing writer recorded
    /// (nw-475, Task 5.2) — only ever populated for a
    /// [`MARKER_REASON_WATCHER_BATCH`] marker today. Empty for every marker
    /// written before this field existed, and for any marker whose
    /// establisher never recorded paths (a plain `{pid}:{nanos}[:{reason}]`
    /// payload still parses; this is additive, not required).
    pub note_paths: Vec<String>,
    /// True when the establishing writer had more in-flight paths than
    /// [`MAX_MARKER_NOTE_PATHS`], so `note_paths` is a prefix, not the full
    /// list.
    pub note_paths_truncated: bool,
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

/// Cap on the number of in-flight note paths recorded in a marker payload
/// (nw-475, Task 5.2): a diagnostic list, not a durability record, so
/// bounded rather than growing with an arbitrarily large watcher batch.
pub const MAX_MARKER_NOTE_PATHS: usize = 20;

/// [`format_marker_payload`], plus a bounded, comma-joined list of in-flight
/// note paths as a FOURTH `:`-delimited field (nw-475, Task 5.2): a leading
/// `1`/`0` truncation flag followed by up to [`MAX_MARKER_NOTE_PATHS`]
/// comma-separated paths, e.g. `1,a.md,b.md` means "more paths were in
/// flight than fit; a.md and b.md are the first two." Falls back to
/// [`format_marker_payload`]'s exact byte shape when `note_paths` is empty,
/// so a caller with nothing in flight to report never grows a field it has
/// nothing to say. Note paths are assumed not to contain a literal `,` or
/// `:` — an unusual path violating that assumption degrades the DIAGNOSTIC
/// list only; the marker's safety-critical fields (pid/timestamp/reason),
/// parsed first and independently, are unaffected.
pub fn format_marker_payload_with_note_paths(
    pid: u32,
    unix_nanos: u128,
    reason: Option<&str>,
    note_paths: &[String],
) -> String {
    if note_paths.is_empty() {
        return format_marker_payload(pid, unix_nanos, reason);
    }
    let truncated = note_paths.len() > MAX_MARKER_NOTE_PATHS;
    let capped = &note_paths[..note_paths.len().min(MAX_MARKER_NOTE_PATHS)];
    let mut paths_field = String::from(if truncated { "1" } else { "0" });
    for path in capped {
        paths_field.push(',');
        paths_field.push_str(path);
    }
    format!(
        "{pid}:{unix_nanos}:{}:{paths_field}\n",
        reason.unwrap_or("")
    )
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
    let (note_paths, note_paths_truncated) = fields
        .next()
        .map(|field| parse_note_paths_field(field.trim()))
        .unwrap_or_default();
    MarkerRecord {
        writer_pid,
        established_unix_nanos,
        reason,
        note_paths,
        note_paths_truncated,
    }
}

/// Parse the fourth `:`-delimited field written by
/// [`format_marker_payload_with_note_paths`]. Never fails: an empty or
/// unrecognised field yields no paths, the same "present but has nothing
/// useful to say" posture the rest of this parser takes toward every other
/// field, and — critically — a marker written before this field existed
/// (no fourth field at all, so `fields.next()` returns `None` upstream)
/// never reaches this function.
fn parse_note_paths_field(field: &str) -> (Vec<String>, bool) {
    if field.is_empty() {
        return (Vec::new(), false);
    }
    let truncated = field.starts_with('1');
    let rest = field.get(1..).unwrap_or("");
    let paths = rest
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (paths, truncated)
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
                    note_paths: Vec::new(),
                    note_paths_truncated: false,
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

    /// nw-475 (Task 5.2): the note-paths field round-trips, including the
    /// truncation flag once the list exceeds the cap.
    #[test]
    fn note_paths_round_trip_through_the_marker_payload() {
        let paths = vec!["a.md".to_string(), "sub/b.md".to_string()];
        let payload = format_marker_payload_with_note_paths(
            9,
            1_755_000_000_000_000_000,
            Some(MARKER_REASON_WATCHER_BATCH),
            &paths,
        );
        let record = parse_marker_payload(&payload);
        assert_eq!(record.writer_pid, Some(9));
        assert_eq!(record.reason.as_deref(), Some(MARKER_REASON_WATCHER_BATCH));
        assert_eq!(record.note_paths, paths);
        assert!(!record.note_paths_truncated);
    }

    /// COUNTERWEIGHT: more paths than the cap are truncated, and the flag
    /// says so.
    #[test]
    fn note_paths_beyond_the_cap_are_truncated_with_the_flag_set() {
        let paths: Vec<String> = (0..(MAX_MARKER_NOTE_PATHS + 5))
            .map(|i| format!("note-{i}.md"))
            .collect();
        let payload = format_marker_payload_with_note_paths(
            9,
            1_755_000_000_000_000_000,
            Some(MARKER_REASON_WATCHER_BATCH),
            &paths,
        );
        let record = parse_marker_payload(&payload);
        assert_eq!(record.note_paths.len(), MAX_MARKER_NOTE_PATHS);
        assert_eq!(record.note_paths, paths[..MAX_MARKER_NOTE_PATHS]);
        assert!(record.note_paths_truncated);
    }

    /// nw-475: an empty `note_paths` slice must not grow the payload at all
    /// — proves the with-note-paths writer is a pure superset of the
    /// original, not a shape a legacy reader might mishandle when nothing
    /// was in flight.
    #[test]
    fn empty_note_paths_falls_back_to_the_plain_payload_shape() {
        let with_empty = format_marker_payload_with_note_paths(
            9,
            1_755_000_000_000_000_000,
            Some(MARKER_REASON_WATCHER_BATCH),
            &[],
        );
        let plain = format_marker_payload(
            9,
            1_755_000_000_000_000_000,
            Some(MARKER_REASON_WATCHER_BATCH),
        );
        assert_eq!(with_empty, plain);
    }

    #[test]
    fn watcher_batch_exception_requires_a_young_timestamp() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let young = parse_marker_payload(&format_marker_payload(
            9,
            now,
            Some(MARKER_REASON_WATCHER_BATCH),
        ));
        assert!(watcher_batch_ranking_exception_applies(&young));

        let ancient = parse_marker_payload(&format_marker_payload(
            9,
            1,
            Some(MARKER_REASON_WATCHER_BATCH),
        ));
        assert!(
            !watcher_batch_ranking_exception_applies(&ancient),
            "a leftover watcher-batch reason from hours ago is not a debounce window"
        );

        let untimestamped = parse_marker_payload("9\n");
        assert!(!watcher_batch_ranking_exception_applies(&untimestamped));
    }

    /// nw-475: backward compatibility — a marker written by a binary before
    /// this field existed (plain `{pid}:{nanos}:{reason}`, no fourth field)
    /// must still parse cleanly, with empty/false note-path values rather
    /// than a parse failure.
    #[test]
    fn a_marker_without_the_note_paths_field_still_parses() {
        let legacy = format_marker_payload(9, 1_755_000_000_000_000_000, Some("cancelled"));
        let record = parse_marker_payload(&legacy);
        assert_eq!(record.writer_pid, Some(9));
        assert_eq!(record.reason.as_deref(), Some("cancelled"));
        assert!(record.note_paths.is_empty());
        assert!(!record.note_paths_truncated);
    }

    /// COUNTERWEIGHT: a legacy TWO-field marker (no reason at all) also
    /// still parses cleanly through the note-paths reader.
    #[test]
    fn a_two_field_legacy_marker_still_parses_with_no_note_paths() {
        let record = parse_marker_payload("4242:1755000000000000000\n");
        assert!(record.note_paths.is_empty());
        assert!(!record.note_paths_truncated);
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
