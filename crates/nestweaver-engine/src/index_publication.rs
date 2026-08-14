//! Liveness-aware view of the `<db>.index-dirty` publication marker, and the
//! predicate that decides whether a publication is *abandoned*.
//!
//! `nestweaver-store` owns the marker's path and payload (see
//! [`nestweaver_store::index_publication`]) but has no `libc` dependency, so it
//! deliberately stops short of asking whether the recorded writer pid is still
//! alive. This module adds that half.
//!
//! Two rules govern everything here.
//!
//! **A publication is abandoned only when the recorded pid is dead AND the
//! in-process lease is unowned.** The pid half is cross-process (the writer is
//! commonly the daemon or a one-shot `nestweaver index`); the lease half closes
//! the case where the writer is *this* process and simply has not finished yet.
//! Either half alone is insufficient.
//!
//! **"Cannot tell" is not "abandoned".** An `EACCES`/`EIO` on the sidecar
//! directory reads as permanently dirty by design and is tested that way
//! (`nestweaver-store/src/db.rs`, `unreadable_index_publication_marker_*`).
//! Every path here maps an undeterminable marker to "do not recover", never to
//! "recover".

use std::path::Path;
use std::time::Duration;

use nestweaver_store::GraphStore;
use nestweaver_store::index_publication::{MarkerRecord, MarkerState, read_marker};

/// Age past which a dirty publication with an *unattributable* writer is
/// reported as wedged rather than transient. A publication window is normally
/// well under a second; anything still dirty after this, with no live writer we
/// can point at, is a diagnosis worth surfacing.
pub const WEDGED_MARKER_AGE: Duration = Duration::from_secs(60);

/// Whether `pid` names a live process.
///
/// `kill(pid, 0)` performs the permission and existence checks without
/// delivering a signal — the established idiom in this tree (`src/main.rs`
/// daemon liveness probes). `EPERM` means the process exists but belongs to
/// another user, which is still *alive*; only `ESRCH` proves it is gone.
///
/// Pid reuse is possible in principle. It is not load-bearing here: a recycled
/// pid can only make this return `true`, which makes recovery *decline*. The
/// failure mode is a missed auto-heal, recoverable with `nestweaver repair`,
/// never a marker cleared out from under a live writer.
#[cfg(unix)]
pub fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Non-unix fallback: we cannot prove the writer is dead, so we never claim it.
/// Auto-heal declines and the operator escape hatch still applies.
#[cfg(not(unix))]
pub fn process_is_alive(_pid: i32) -> bool {
    true
}

/// File-derived publication state, suitable for reporting on the direct
/// (`--no-daemon`) path with no daemon and no writable store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPublicationStatus {
    /// True when ranked queries fail closed — present OR undeterminable.
    pub dirty: bool,
    /// False when the marker's state could not be determined at all.
    pub determinable: bool,
    /// Pid recorded in the marker payload, when it parsed.
    pub writer_pid: Option<i32>,
    /// Whether that pid still names a live process. `None` when there is no
    /// pid to check.
    pub writer_alive: Option<bool>,
    /// Seconds since the marker was established, per its own payload.
    pub marker_age_s: Option<u64>,
    /// Reason recorded by the writer, when it recorded one (see
    /// [`nestweaver_store::index_publication::MARKER_REASON_CANCELLED`]).
    pub writer_reason: Option<String>,
    /// The marker path, so a message can name the exact file.
    pub marker_path: String,
}

impl IndexPublicationStatus {
    /// A dirty publication that is *not* plausibly in flight: the writer is
    /// provably dead, or nothing can be attributed to a live writer and the
    /// marker is older than [`WEDGED_MARKER_AGE`].
    ///
    /// An undeterminable marker is reported as wedged (something is wrong and a
    /// human should look), but see [`abandoned_writer_pid`] — being *wedged* is
    /// not sufficient to be *recoverable*.
    pub fn is_wedged(&self) -> bool {
        if !self.dirty {
            return false;
        }
        if !self.determinable {
            return true;
        }
        match self.writer_alive {
            Some(true) => false,
            Some(false) => true,
            None => self
                .marker_age_s
                .is_some_and(|age| Duration::from_secs(age) >= WEDGED_MARKER_AGE),
        }
    }

    /// The operator escape hatch to name in a wedged-state message.
    pub fn repair_command(db_path: &Path) -> String {
        format!("nestweaver repair --db {}", db_path.display())
    }
}

/// Read the marker for `db_path` and decorate it with writer liveness.
pub fn status(db_path: &Path) -> IndexPublicationStatus {
    status_from(db_path, read_marker(db_path))
}

/// [`status`] against an already-read [`MarkerState`], so callers that already
/// hold one do not re-read the file.
pub fn status_from(db_path: &Path, state: MarkerState) -> IndexPublicationStatus {
    let marker_path = nestweaver_store::index_publication::marker_path(db_path)
        .display()
        .to_string();
    match state {
        MarkerState::Absent => IndexPublicationStatus {
            dirty: false,
            determinable: true,
            writer_pid: None,
            writer_alive: None,
            marker_age_s: None,
            writer_reason: None,
            marker_path,
        },
        MarkerState::Undeterminable(_) => IndexPublicationStatus {
            dirty: true,
            determinable: false,
            writer_pid: None,
            writer_alive: None,
            marker_age_s: None,
            writer_reason: None,
            marker_path,
        },
        MarkerState::Present(record) => IndexPublicationStatus {
            dirty: true,
            determinable: true,
            writer_pid: record.writer_pid,
            writer_alive: record.writer_pid.map(process_is_alive),
            marker_age_s: record.age().map(|age| age.as_secs()),
            writer_reason: record.reason.clone(),
            marker_path,
        },
    }
}

/// The pid of a writer that is provably gone, when this marker names one.
///
/// `None` means "do not recover", for any of four distinct reasons: the marker
/// is absent, it is undeterminable, it records no pid we can attribute, or the
/// pid it records is still alive. Only a *present, readable, attributed, dead*
/// writer qualifies.
pub fn abandoned_writer_pid(state: &MarkerState) -> Option<i32> {
    let record: &MarkerRecord = state.record()?;
    let pid = record.writer_pid?;
    (!process_is_alive(pid)).then_some(pid)
}

/// The full abandoned-publication predicate: a dead recorded writer AND an
/// unowned in-process lease.
///
/// The lease half matters when the recorded pid belongs to a process that
/// exited while *this* process still holds a lease for the same store — and,
/// more importantly, when the marker was written by a previous incarnation but
/// a live publisher in this process has already taken over.
pub fn publication_is_abandoned(store: &GraphStore, state: &MarkerState) -> Option<i32> {
    let pid = abandoned_writer_pid(state)?;
    store.index_publication_lease_is_unowned().then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_store::index_publication::{
        MARKER_REASON_CANCELLED, format_marker_payload, marker_path,
    };

    fn present(pid: Option<i32>, reason: Option<&str>) -> MarkerState {
        MarkerState::Present(MarkerRecord {
            writer_pid: pid,
            established_unix_nanos: None,
            reason: reason.map(str::to_string),
        })
    }

    #[test]
    fn this_process_is_alive() {
        assert!(process_is_alive(std::process::id() as i32));
    }

    #[test]
    fn a_nonpositive_pid_is_never_alive() {
        assert!(!process_is_alive(0));
        assert!(!process_is_alive(-1));
    }

    #[test]
    fn a_live_writer_is_never_abandoned() {
        let state = present(Some(std::process::id() as i32), None);
        assert_eq!(abandoned_writer_pid(&state), None);
    }

    #[test]
    fn an_undeterminable_marker_is_never_abandoned_but_is_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let state = MarkerState::Undeterminable("permission denied".into());
        assert_eq!(
            abandoned_writer_pid(&state),
            None,
            "cannot tell must never mean abandoned"
        );
        let status = status_from(&db_path, state);
        assert!(status.dirty);
        assert!(!status.determinable);
        assert!(status.is_wedged());
    }

    #[test]
    fn an_unattributed_marker_is_never_abandoned() {
        assert_eq!(abandoned_writer_pid(&present(None, None)), None);
    }

    #[test]
    fn an_absent_marker_is_clean_and_not_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let status = status(&db_path);
        assert!(!status.dirty);
        assert!(status.determinable);
        assert!(!status.is_wedged());
    }

    #[test]
    fn a_live_writer_reads_as_transient_not_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        std::fs::write(
            marker_path(&db_path),
            format_marker_payload(std::process::id(), 1, None),
        )
        .unwrap();
        let status = status(&db_path);
        assert_eq!(status.writer_alive, Some(true));
        assert!(!status.is_wedged(), "a live publication is not wedged");
    }

    #[test]
    fn publication_is_abandoned_requires_both_halves() {
        let store = GraphStore::in_memory().unwrap();
        let dead = {
            let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
            let pid = child.id() as i32;
            child.wait().unwrap();
            pid
        };
        let dead_state = present(Some(dead), None);
        assert_eq!(
            publication_is_abandoned(&store, &dead_state),
            Some(dead),
            "dead writer + unowned lease is the abandoned case"
        );

        let held = store.acquire_index_publication_lease().unwrap();
        assert_eq!(
            publication_is_abandoned(&store, &dead_state),
            None,
            "a held lease means a live publisher took over in this process"
        );
        drop(held);

        assert_eq!(
            publication_is_abandoned(&store, &present(Some(std::process::id() as i32), None)),
            None,
            "a live writer is never abandoned even with an unowned lease"
        );
    }

    #[test]
    fn status_reports_the_cancelled_reason() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        std::fs::write(
            marker_path(&db_path),
            format_marker_payload(std::process::id(), 1, Some(MARKER_REASON_CANCELLED)),
        )
        .unwrap();
        assert_eq!(
            status(&db_path).writer_reason.as_deref(),
            Some(MARKER_REASON_CANCELLED)
        );
    }
}
