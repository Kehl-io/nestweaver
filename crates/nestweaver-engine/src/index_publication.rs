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
//! **Writer ownership, not PID liveness, is authoritative.** A writable
//! [`GraphStore`] proves that no other process owns lbug's exclusive database
//! writer lock; the in-process publication lease proves that no publisher in
//! this process owns the publication. Marker PIDs remain useful diagnostics,
//! but may be recycled and therefore cannot veto recovery by a proven writer.
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

/// Best-effort process-liveness evidence carried only for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLiveness {
    Alive,
    Dead,
    Unknown,
}

/// Classify whether `pid` names a live process.
///
/// `kill(pid, 0)` performs the permission and existence checks without
/// delivering a signal — the established idiom in this tree (`src/main.rs`
/// daemon liveness probes). `EPERM` means the process exists but belongs to
/// another user, which is still *alive*; only `ESRCH` proves it is gone.
///
/// PID reuse is expected and this result is deliberately not an ownership
/// predicate. Writable recovery is authorized by the database writer lock and
/// the in-process publication lease; this value only enriches status output.
#[cfg(unix)]
pub fn process_liveness(pid: i32) -> ProcessLiveness {
    if pid <= 0 {
        return ProcessLiveness::Dead;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return ProcessLiveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ProcessLiveness::Dead,
        // POSIX specifies EPERM only when the target exists but cannot be
        // signalled by this caller, so it is positive liveness evidence.
        Some(libc::EPERM) => ProcessLiveness::Alive,
        _ => ProcessLiveness::Unknown,
    }
}

/// Non-unix fallback: the marker PID supplies no trustworthy ownership proof.
#[cfg(not(unix))]
pub fn process_liveness(_pid: i32) -> ProcessLiveness {
    ProcessLiveness::Unknown
}

/// Compatibility predicate for diagnostic callers. Unknown remains
/// fail-closed and therefore reads as possibly alive.
pub fn process_is_alive(pid: i32) -> bool {
    process_liveness(pid) != ProcessLiveness::Dead
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
    /// pid to check or liveness is indeterminate.
    pub writer_alive: Option<bool>,
    /// True when a marker PID exists but the platform could not classify it.
    pub writer_liveness_unknown: bool,
    /// Whether the canonical `<db>.write.lock` is currently held. This is the
    /// ownership signal; marker PID liveness above is diagnostic only.
    pub writer_authority_held: Option<bool>,
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
        match self.writer_authority_held {
            Some(true) => false,
            // A free canonical lease proves no writer can finish this marker,
            // even when its diagnostic PID has since been recycled.
            Some(false) => true,
            // Unknown ownership remains fail-closed and needs operator
            // attention; PID liveness cannot turn it into verified ownership.
            None => true,
        }
    }

    /// True when the marker bytes themselves cannot be determined and an
    /// operator must explicitly authorize discarding unknown state. A readable
    /// PID-less marker does not require force: exact database authority and the
    /// process-local publication lease are the ownership proof.
    pub fn needs_forced_repair(&self) -> bool {
        self.is_wedged() && !self.determinable
    }

    /// The operator escape hatch to name in a wedged-state message.
    ///
    /// Only unreadable/undeterminable marker state needs the explicit override;
    /// readable markers are reconciled under exact writer authority.
    pub fn repair_command(db_path: &Path) -> String {
        format!("nestweaver repair --db {}", db_path.display())
    }

    /// The repair command appropriate to THIS status.
    pub fn repair_command_for(&self, db_path: &Path) -> String {
        if self.needs_forced_repair() {
            format!("nestweaver repair --db {} --force", db_path.display())
        } else {
            Self::repair_command(db_path)
        }
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
    let writer_authority_held = match nestweaver_store::write_lease_state(db_path) {
        nestweaver_store::WriteLeaseState::Held => Some(true),
        nestweaver_store::WriteLeaseState::Free => Some(false),
        nestweaver_store::WriteLeaseState::Unknown => None,
    };
    match state {
        MarkerState::Absent => IndexPublicationStatus {
            dirty: false,
            determinable: true,
            writer_pid: None,
            writer_alive: None,
            writer_liveness_unknown: false,
            writer_authority_held,
            marker_age_s: None,
            writer_reason: None,
            marker_path,
        },
        MarkerState::Undeterminable(_) => IndexPublicationStatus {
            dirty: true,
            determinable: false,
            writer_pid: None,
            writer_alive: None,
            writer_liveness_unknown: false,
            writer_authority_held,
            marker_age_s: None,
            writer_reason: None,
            marker_path,
        },
        MarkerState::Present(record) => {
            let liveness = record.writer_pid.map(process_liveness);
            IndexPublicationStatus {
                dirty: true,
                determinable: true,
                writer_pid: record.writer_pid,
                writer_alive: match liveness {
                    Some(ProcessLiveness::Alive) => Some(true),
                    Some(ProcessLiveness::Dead) => Some(false),
                    Some(ProcessLiveness::Unknown) | None => None,
                },
                writer_liveness_unknown: liveness == Some(ProcessLiveness::Unknown),
                writer_authority_held,
                marker_age_s: record.age().map(|age| age.as_secs()),
                writer_reason: record.reason.clone(),
                marker_path,
            }
        }
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
    (process_liveness(pid) == ProcessLiveness::Dead).then_some(pid)
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
    fn live_marker_pid_is_diagnostic_and_free_authority_is_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let state = present(Some(std::process::id() as i32), None);
        let status = status_from(&db_path, state);
        assert_eq!(status.writer_alive, Some(true));
        assert_eq!(status.writer_authority_held, Some(false));
        assert!(status.is_wedged());

        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        let held = status_from(&db_path, present(Some(std::process::id() as i32), None));
        assert_eq!(held.writer_authority_held, Some(true));
        assert!(!held.is_wedged());
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
        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        let status = status(&db_path);
        assert_eq!(status.writer_alive, Some(true));
        assert!(!status.is_wedged(), "a live publication is not wedged");
    }

    #[test]
    fn publication_is_abandoned_requires_both_halves() {
        let store = GraphStore::in_memory().unwrap();
        let dead = {
            // nw-138: resolve `true` via PATH. macOS ships it at /usr/bin/true
            // and has no /bin/true, so hardcoding the path panicked with
            // NotFound on every macOS machine while passing in Linux CI.
            let mut child = std::process::Command::new("true").spawn().unwrap();
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
    fn an_unattributed_marker_is_wedged_but_needs_no_force() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        // The legacy / `touch`-created marker: present, but nothing to attribute.
        std::fs::write(marker_path(&db_path), b"dirty").unwrap();
        let status = status(&db_path);
        assert!(status.dirty);
        assert_eq!(status.writer_pid, None);
        assert_eq!(status.writer_alive, None);
        assert_eq!(status.marker_age_s, None);
        assert!(
            status.is_wedged(),
            "an unattributable marker cannot clear itself; reporting it as \
             transient promises a recovery that never arrives"
        );
        assert!(
            !status.needs_forced_repair(),
            "exact writer authority can reconcile a readable marker without PID attribution"
        );
    }

    #[test]
    fn the_named_repair_command_matches_what_the_case_actually_needs() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");

        std::fs::write(marker_path(&db_path), b"dirty").unwrap();
        assert!(
            !status(&db_path)
                .repair_command_for(&db_path)
                .ends_with("--force"),
            "a readable unattributed marker needs exact authority, not an override"
        );

        let dead = {
            // nw-138: resolve `true` via PATH. macOS ships it at /usr/bin/true
            // and has no /bin/true, so hardcoding the path panicked with
            // NotFound on every macOS machine while passing in Linux CI.
            let mut c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id() as i32;
            c.wait().unwrap();
            pid
        };
        std::fs::write(
            marker_path(&db_path),
            format_marker_payload(dead as u32, 1, None),
        )
        .unwrap();
        assert!(
            !status(&db_path)
                .repair_command_for(&db_path)
                .ends_with("--force"),
            "a provably-dead attributed writer needs no override"
        );
    }

    #[test]
    fn an_undeterminable_marker_needs_forced_repair() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let status = status_from(&db_path, MarkerState::Undeterminable("EACCES".into()));
        assert!(status.is_wedged());
        assert!(status.needs_forced_repair());
    }

    #[test]
    fn an_attributed_live_publication_never_needs_forced_repair() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        std::fs::write(
            marker_path(&db_path),
            format_marker_payload(std::process::id(), 1, None),
        )
        .unwrap();
        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        let status = status(&db_path);
        assert!(!status.is_wedged());
        assert!(!status.needs_forced_repair());
    }

    #[test]
    fn a_young_timestamped_marker_without_authority_is_already_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // A fresh timestamp and PID are diagnostic only. With the canonical
        // authority free, no writer can complete this publication.
        std::fs::write(marker_path(&db_path), format!("x:{now}\n")).unwrap();
        let status = status(&db_path);
        assert_eq!(status.writer_pid, None);
        assert!(status.marker_age_s.is_some());
        assert!(status.is_wedged());
        assert!(!status.needs_forced_repair());
    }

    #[test]
    fn a_transient_publication_is_never_told_to_force_repair() {
        // The third-consumer trap: a caller of `repair_command_for` that does
        // NOT gate on `is_wedged()` first must still never print `--force` at
        // a publication that is simply in flight — here a young, timestamped,
        // pid-less marker, which `is_wedged()` classifies as transient.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::fs::write(marker_path(&db_path), format!("x:{now}\n")).unwrap();
        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        let status = status(&db_path);
        assert_eq!(status.writer_pid, None);
        assert!(status.marker_age_s.is_some());
        assert!(!status.is_wedged(), "a young marker is transient");
        assert!(
            !status.needs_forced_repair(),
            "a transient publication does not need repair at all, forced or otherwise"
        );
        assert!(
            !status.repair_command_for(&db_path).ends_with("--force"),
            "an ungated consumer must not be handed --force for a transient publication"
        );
    }

    #[test]
    fn an_aged_out_pidless_marker_is_wedged_but_still_needs_no_force() {
        // Once the same pid-less marker is old it is wedged, but readable bytes
        // plus exact database authority remain sufficient for automatic repair.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            - (WEDGED_MARKER_AGE + Duration::from_secs(60)).as_nanos();
        std::fs::write(marker_path(&db_path), format!("x:{old}\n")).unwrap();
        let status = status(&db_path);
        assert_eq!(status.writer_pid, None);
        assert!(status.is_wedged(), "an aged-out pid-less marker is wedged");
        assert!(!status.needs_forced_repair());
        assert!(!status.repair_command_for(&db_path).ends_with("--force"));
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
