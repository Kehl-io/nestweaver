//! Byte-level progress reporting for artifact resolution (nw-484).
//!
//! Exposes a small, hf-hub-independent sink trait so callers (the daemon's
//! seed thread) don't need to depend on `hf_hub::progress` directly. Internally,
//! [`HfProgressAdapter`] implements `hf_hub::progress::ProgressHandler` and
//! translates hf-hub's rich, per-operation event stream into a single running
//! `(done, total)` byte pair accumulated across every file
//! `resolve_model_artifacts_with_progress` resolves in one call.

use std::sync::Arc;
use std::sync::Mutex;

use hf_hub::progress::{DownloadEvent, ProgressEvent, ProgressHandler};

/// Receives cumulative byte progress for one `resolve_model_artifacts_with_progress`
/// call.
///
/// `done`/`total` are cumulative across every artifact file resolved so far in
/// the call, not per-file. `total` grows as additional files are started (the
/// grand total is not known upfront — the artifact set is resolved file by
/// file), so treat it as a lower bound on the eventual total, not a fixed
/// target. `total >= done` always holds — see [`HfProgressAdapter`] for why
/// that isn't automatic. hf-hub's `ProgressHandler` contract applies to
/// `on_bytes` too: it must not block and must not panic — it may be called
/// from hf-hub's background transfer machinery.
pub trait ArtifactProgressSink: Send + Sync {
    fn on_bytes(&self, done: u64, total: u64);
}

/// Mutable state behind the single `Mutex` in [`HfProgressAdapter`].
struct AdapterState {
    /// Bytes folded in from every FINISHED artifact file (i.e. every prior
    /// `download_file()` call in this `resolve_model_artifacts_with_progress`
    /// pass).
    completed_bytes: u64,
    /// Bytes total/done for the CURRENTLY in-flight operation only. Reset on
    /// every `Start` and folded into `completed_bytes` on `Complete`.
    current_total: u64,
    current_done: u64,
    /// True strictly between a `Start` and the matching `Complete` for the
    /// operation currently in flight. Progress/AggregateProgress events that
    /// arrive while this is false are ignored — see the "straggler" note on
    /// [`HfProgressAdapter`].
    active: bool,
}

/// Adapts an [`ArtifactProgressSink`] to hf-hub's [`ProgressHandler`].
///
/// # Why a single `Mutex`, not independent atomics
///
/// hf-hub's xet transfer path spawns its own background poller and, on the
/// caller side, ABORTS it without joining when the surrounding future
/// completes (xet.rs, `~130-197`/`~610-697` in the pinned 1.0.0 source) —
/// there is no guarantee a straggler `emit` from that poller cannot still be
/// in flight after `resolve_model_artifacts_with_builder` has already moved
/// on to the NEXT `download_file()` call. With independent atomics, such a
/// straggler could write into `current_total`/`current_done` after this
/// adapter's `Complete` handler has already reset them for the next file,
/// corrupting the running total with no way to tell "this file's real
/// progress" from "a dead file's late echo". A single lock makes every
/// transition (`Start` resets, `Progress` updates, `Complete` folds)
/// linearize against every other transition, and the `active` flag (see
/// [`AdapterState`]) makes a straggler's event — arriving in the dead zone
/// between one `Complete` and the next `Start` — a deliberate no-op instead
/// of undefined interleaving. The lock is uncontended in the common case (one
/// artifact resolves at a time, synchronously, from this crate's blocking
/// client), so this costs nothing observable.
///
/// # Why totals can be zero, and why that must never regress `done`
///
/// hf-hub reports `total_bytes: 0` on `Start`/`Progress` whenever neither
/// `Content-Length` nor `X-Linked-Size` is present on the response
/// (`repository/download.rs` `~492-495`), which `report()` (via
/// `total.max(done)`) and the `Complete` handler (via `total.max(current_done)`
/// when folding into `completed_bytes`) both guard against — otherwise an
/// unknown-size file would report `done > total` while active, and its
/// bytes would be LOST entirely (not merely under-reported) once `Complete`
/// folded in a zero `current_total` instead of what was actually
/// transferred.
pub(crate) struct HfProgressAdapter {
    sink: Arc<dyn ArtifactProgressSink>,
    state: Mutex<AdapterState>,
}

impl HfProgressAdapter {
    pub(crate) fn new(sink: Arc<dyn ArtifactProgressSink>) -> Self {
        Self {
            sink,
            state: Mutex::new(AdapterState {
                completed_bytes: 0,
                current_total: 0,
                current_done: 0,
                active: false,
            }),
        }
    }

    /// Reads the current cumulative totals and reports them to the sink.
    /// Computes the snapshot under the lock, then calls the sink AFTER
    /// releasing it — the sink contract forbids blocking, but nothing
    /// requires it to be reentrant into this adapter, and calling
    /// unfamiliar code while holding our own lock is needless risk.
    fn report(&self) {
        let (done, total) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let done = state.completed_bytes + state.current_done;
            // Never let an unknown/short-reported current total make the
            // cumulative total look smaller than what has actually moved.
            let total = (state.completed_bytes + state.current_total).max(done);
            (done, total)
        };
        self.sink.on_bytes(done, total);
    }
}

impl ProgressHandler for HfProgressAdapter {
    fn on_progress(&self, event: &ProgressEvent) {
        let ProgressEvent::Download(event) = event else {
            // This adapter is only ever attached to download operations; an
            // upload event here would mean a caller reused it incorrectly.
            return;
        };
        match event {
            DownloadEvent::Start { total_bytes, .. } => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.current_total = *total_bytes;
                state.current_done = 0;
                state.active = true;
            }
            DownloadEvent::Progress { files } => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !state.active {
                    // A straggler from an already-completed operation (see
                    // the struct docs) — deliberately ignored rather than
                    // corrupting the next file's in-flight counters.
                    drop(state);
                    return;
                }
                // Single-file `download_file()` calls report exactly one
                // file per delta; the last entry is the authoritative
                // in-flight state for this operation.
                if let Some(file) = files.last() {
                    state.current_done = file.bytes_completed;
                    if file.total_bytes > 0 {
                        state.current_total = file.total_bytes;
                    }
                }
                drop(state);
            }
            DownloadEvent::AggregateProgress {
                bytes_completed,
                total_bytes,
                ..
            } => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !state.active {
                    drop(state);
                    return;
                }
                state.current_done = *bytes_completed;
                state.current_total = *total_bytes;
                drop(state);
            }
            DownloadEvent::Complete => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !state.active {
                    // `Complete` with no preceding `Start` in this adapter's
                    // lifetime: hf-hub emits this for a commit-hash-PINNED
                    // file that is already cached (`repository/download.rs`
                    // `~408-413`) — the fast path skips the HEAD round-trip
                    // that `Start` is fired from entirely. Dormant today:
                    // `local.rs` never pins a `revision`, so every resolve in
                    // this crate goes through the branch that always emits
                    // `Start` first. Kept as a harmless no-op rather than an
                    // assumption that would break silently if that changes.
                    drop(state);
                    self.report();
                    return;
                }
                // Fold whichever of (reported total, actually-transferred
                // done) is larger — an unknown-size file (`current_total ==
                // 0`) must still have its real bytes counted, never dropped.
                let folded = state.current_total.max(state.current_done);
                state.completed_bytes += folded;
                state.current_total = 0;
                state.current_done = 0;
                state.active = false;
                drop(state);
            }
        }
        self.report();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hf_hub::progress::{FileProgress, FileStatus};
    use std::sync::Mutex as StdMutex;

    /// Records every `on_bytes` call, offline — no network, no hf-hub client.
    /// This is the "unit-level fake progress source" verifying the adapter's
    /// event-to-callback translation in isolation.
    struct RecordingSink {
        calls: StdMutex<Vec<(u64, u64)>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(u64, u64)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ArtifactProgressSink for RecordingSink {
        fn on_bytes(&self, done: u64, total: u64) {
            self.calls.lock().unwrap().push((done, total));
        }
    }

    fn file_progress(bytes_completed: u64, total_bytes: u64, status: FileStatus) -> FileProgress {
        FileProgress {
            filename: "artifact".to_string(),
            bytes_completed,
            total_bytes,
            status,
        }
    }

    /// One file, start to finish: `Start` reports the known total with zero
    /// done, `Progress` deltas move `done` up, `Complete` folds the file's
    /// bytes into the running total without changing it (a fully-downloaded
    /// file's done/total do not regress).
    #[test]
    fn single_file_lifecycle_reports_monotonic_progress() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 1000,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(400, 1000, FileStatus::InProgress)],
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(1000, 1000, FileStatus::Complete)],
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Complete));

        assert_eq!(
            sink.calls(),
            vec![(0, 1000), (400, 1000), (1000, 1000), (1000, 1000)],
        );
    }

    /// Two files resolved in sequence (as `resolve_model_artifacts_with_progress`
    /// does — one `download_file()` call per artifact): the SECOND file's
    /// `Start` must not reset the first file's already-completed bytes, and
    /// the reported total keeps growing as each new file is discovered.
    #[test]
    fn sequential_files_accumulate_without_regressing() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        // File 1: 100 bytes, resolves fully.
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 100,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(100, 100, FileStatus::Complete)],
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Complete));

        // File 2: 400 bytes, partially in flight.
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 400,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(150, 400, FileStatus::InProgress)],
        }));

        let calls = sink.calls();
        // After file 1 completes: done == total == 100.
        assert!(calls.contains(&(100, 100)));
        // File 2's Start must not regress the running total below file 1's
        // 100 completed bytes.
        assert!(calls.contains(&(100, 500)), "{calls:?}");
        // File 2's in-flight progress adds on top of file 1's completed bytes.
        assert_eq!(*calls.last().unwrap(), (250, 500));
    }

    /// A `download_file()` cache hit still reports a real `total_bytes` on
    /// its `Progress` delta (`repository/download.rs` `~537-547`: the cached
    /// path uses the known local file size, not zero). A `total_bytes: 0`
    /// delta is therefore not the normal cache-hit shape in this crate — it
    /// is hf-hub's unknown-size fallback (`~492-495`, no Content-Length/
    /// X-Linked-Size header) — but the guard is kept regardless: whichever
    /// produces it, a zero total_bytes delta must never erase a nonzero
    /// total `Start` already reported.
    #[test]
    fn zero_sized_progress_delta_does_not_erase_a_known_total() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 2048,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(2048, 0, FileStatus::Complete)],
        }));

        let (done, total) = *sink.calls().last().unwrap();
        assert_eq!(done, 2048);
        assert_eq!(
            total, 2048,
            "a zero total_bytes delta must not erase Start's known total"
        );
    }

    /// An unknown-size file end to end: hf-hub reports `total_bytes: 0` on
    /// BOTH `Start` and every `Progress` delta (no Content-Length or
    /// X-Linked-Size header at all — `repository/download.rs` `~492-495`).
    /// `done` must still climb monotonically while active, `total` must
    /// never read less than `done` at any point, and `Complete` must fold
    /// the REAL transferred bytes into `completed_bytes` — not the
    /// still-zero `current_total` — or a whole file's bytes would silently
    /// vanish from the running total instead of merely being
    /// under-reported.
    #[test]
    fn unknown_size_file_does_not_regress_or_lose_bytes() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 0,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(500, 0, FileStatus::InProgress)],
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(1_200, 0, FileStatus::InProgress)],
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Complete));

        let calls = sink.calls();
        for &(done, total) in &calls {
            assert!(
                total >= done,
                "total must never read less than done: {calls:?}"
            );
        }
        let done_sequence: Vec<u64> = calls.iter().map(|&(done, _)| done).collect();
        assert!(
            done_sequence.windows(2).all(|pair| pair[1] >= pair[0]),
            "done must never regress: {done_sequence:?}"
        );
        // The 1200 bytes actually transferred must survive into the final
        // completed total, not the (still-zero) reported total.
        assert_eq!(*calls.last().unwrap(), (1_200, 1_200));

        // A second, known-size file after the unknown-size one must add on
        // top of the 1200 bytes just folded in, proving nothing was lost.
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 300,
        }));
        assert_eq!(*sink.calls().last().unwrap(), (1_200, 1_500));
    }

    /// A straggler event from hf-hub's un-joined xet poller (see the
    /// `HfProgressAdapter` struct docs) arriving AFTER `Complete` but before
    /// the next `Start` must be ignored, not corrupt the completed total.
    #[test]
    fn a_late_progress_event_after_complete_is_ignored() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 100,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(100, 100, FileStatus::Complete)],
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Complete));

        // A straggler from the same (now-finished) operation's aborted xet
        // poller — must be a no-op.
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![file_progress(999, 999, FileStatus::InProgress)],
        }));

        assert_eq!(
            *sink.calls().last().unwrap(),
            (100, 100),
            "a straggler event after Complete must not corrupt the completed total"
        );
    }

    /// The dormant gap (fix 5): `Complete` with no preceding `Start` — hf-hub
    /// emits this for a commit-hash-pinned, already-cached file
    /// (`repository/download.rs` `~408-413`). Unreachable through this
    /// crate today (`local.rs` never pins a revision), but must stay a
    /// harmless no-op rather than corrupting `completed_bytes` if that ever
    /// changes.
    #[test]
    fn complete_with_no_preceding_start_is_a_harmless_no_op() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Complete));
        assert_eq!(sink.calls(), vec![(0, 0)]);

        // A subsequent real file still resolves normally afterward.
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 50,
        }));
        assert_eq!(*sink.calls().last().unwrap(), (0, 50));
    }

    /// The xet aggregate-progress channel (batched multi-file transfers)
    /// reports its own done/total pair directly, independent of the
    /// per-file `Progress` channel.
    #[test]
    fn aggregate_progress_reports_batch_totals() {
        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
            total_files: 1,
            total_bytes: 20_000,
        }));
        adapter.on_progress(&ProgressEvent::Download(DownloadEvent::AggregateProgress {
            bytes_completed: 5_000,
            total_bytes: 20_000,
            bytes_per_sec: Some(1_000.0),
        }));

        assert_eq!(*sink.calls().last().unwrap(), (5_000, 20_000));
    }

    /// An upload event reaching this adapter (should never happen in
    /// practice — it is only attached to download builders) is ignored
    /// rather than panicking or mis-reporting.
    #[test]
    fn upload_events_are_ignored() {
        use hf_hub::progress::UploadEvent;

        let sink = Arc::new(RecordingSink::new());
        let adapter = HfProgressAdapter::new(sink.clone() as Arc<dyn ArtifactProgressSink>);

        adapter.on_progress(&ProgressEvent::Upload(UploadEvent::Complete));

        assert!(sink.calls().is_empty());
    }
}
