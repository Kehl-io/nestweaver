//! The shutdown drain: wait for in-flight writes and indexing to finish, then
//! broadcast shutdown.
//!
//! This module holds the drain LOOP; the daemon owns the triggers (the gRPC
//! `Shutdown` RPC and the SIGTERM handler) and the state the loop reads. It
//! lives here, next to the worker pool, because the drain's exit condition is
//! about work the worker publishes — a unit test in this crate can drive a
//! real worker-pool index against the real drain, which the daemon crate
//! cannot (its tests link this crate without the `cfg(test)` `file://` clone
//! allowance, so no daemon test can run a real index job).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The counters and the shutdown channel the drain loop reads. Every field is
/// a shared handle: the daemon clones its own `Arc`s in, so the loop observes
/// the same values the RPC handlers and the worker pool mutate.
///
/// CONSUMER CONTRACT. This loop only OBSERVES and reports; the guarantees its
/// log messages assert belong to the consumer, and [`run_drain`] cannot
/// enforce them:
///
///  - "NEW writes are already being refused with UNAVAILABLE" — the consumer
///    must refuse new writes before and for the whole drain (the daemon does
///    this via `ConnectionGuard::write`, and `begin_shutdown_drain` sets
///    `drained` before spawning this loop). Without that, fresh work can
///    extend the wait indefinitely while the log claims it cannot.
///  - The `embed` / `plan_embed` write-gate behaviour the write-branch report
///    describes is the daemon's RPC-gating design, not something this loop
///    does.
///  - The operator commands named in the messages (`nestweaver daemon stop
///    [--force]`, `kill -9`) are the daemon's CLI; a different consumer must
///    substitute its own escalation path rather than inherit these strings.
pub struct DrainSignals {
    /// In-flight write RPCs. A write holds the DB write lock on an
    /// unabortable `spawn_blocking` thread, so while any are running the wait
    /// is unbounded.
    pub active_writes: Arc<AtomicU32>,
    /// The worker pool's `indexing_active` flag. A flag, not a proof of work:
    /// it cannot clear once the pool is drained with a non-empty queue.
    pub indexing_active: Arc<AtomicBool>,
    /// The worker pool's OWN count of spawned index jobs still running —
    /// incremented when a job is claimed, decremented only when its task
    /// finishes (commit included). Unlike the flag, this is a proof of work:
    /// it is how the drain tells a genuinely running index job (unbounded
    /// wait, like a write) from a stuck `indexing_active` flag (bounded by
    /// the ceiling).
    pub indexing_in_flight: Arc<AtomicU32>,
    /// The channel the drain broadcasts on when the wait ends. Every listener
    /// accepts until this fires, so the broadcast is what ends read service.
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
}

/// Minimum spacing between repeats of the over-ceiling drain warning. The
/// ceiling itself sets the cadence; this floor stops a tiny
/// `NESTWEAVER_DRAIN_TIMEOUT_SECS` from turning the log into a spinner.
const DRAIN_OVER_CEILING_REPORT_FLOOR_SECS: u64 = 60;

/// Wait for in-flight writes and indexing to finish, then broadcast shutdown.
///
/// `ceiling` (`NESTWEAVER_DRAIN_TIMEOUT_SECS`) is a REPORTING threshold, not a
/// kill switch, and this function must not pretend otherwise. Nothing in this
/// process can abort an in-flight write: daemon writes run on `spawn_blocking`
/// threads Tokio cannot cancel (see the "`spawn_blocking` work cannot be
/// aborted" note on the daemon's exit path), and the shutdown broadcast this
/// function ends with only stops listeners ACCEPTING — it cannot preempt work
/// already running.
///
/// So the loop keeps waiting past the ceiling and says so. It used to log
/// "drain timeout reached — forcing shutdown" and break, which was false twice
/// over:
///
///  - nothing was forced. The broadcast does not abort the write, the process
///    stayed alive holding the DB write lock, and only an operator's SIGKILL
///    ever ended it; and
///  - broadcasting shutdown there tore down every listener while the process
///    lived on, which is how a stuck WRITE drain also took READS down. Almost
///    no read needs the write gate (the daemon's `ConnectionGuard::read` does
///    not take `write_mutex`), so they were not blocked by the drain itself —
///    they died because the UDS/TCP/MCP acceptors had already been shut down
///    by that premature broadcast, and every new `daemon status` / MCP / CLI
///    read connection was refused for as long as the write ran.
///
/// Waiting instead keeps the daemon readable until the operator escalates —
/// and if the write does finish, shutdown still completes cleanly rather than
/// leaving a half-dead process behind. "Readable" is not absolute: `embed` and
/// `plan_embed` take `write_mutex` themselves, so they stay blocked for the
/// duration of the stuck write, and the ceiling message says so.
///
/// The unbounded wait applies to work that is GENUINELY in flight: writes,
/// and index jobs the worker pool's own in-flight counter says are running.
/// `indexing_active` on its own — the flag set with no job in flight — stays
/// bounded by the ceiling: see the comment on that branch for why waiting on
/// a stuck flag forever would be a hang rather than a safeguard.
///
/// What the in-flight counter can and cannot detect: it proves a job's task
/// has not finished, not that the job is making progress. A job that is
/// in-flight but truly wedged is indistinguishable from a slow one, so it
/// earns the same unbounded wait a stuck write gets — reads stay served, the
/// over-ceiling report keeps naming the operator escapes (`daemon stop
/// --force` / `kill -9`), and nothing escalates automatically. That is
/// strictly better than the old bounded branch, which broadcast at the
/// ceiling and left the same wedged job running with NOTHING being served.
pub async fn run_drain(signals: DrainSignals, ceiling: u64) {
    let ceiling_at = std::time::Duration::from_secs(ceiling);
    let half = std::time::Duration::from_secs(ceiling / 2);
    let ninety = std::time::Duration::from_secs(ceiling * 9 / 10);
    let repeat_every =
        std::time::Duration::from_secs(ceiling.max(DRAIN_OVER_CEILING_REPORT_FLOOR_SECS));
    let start = tokio::time::Instant::now();
    let mut warned_half = false;
    let mut warned_ninety = false;
    // When the next over-ceiling report is due: first at the ceiling itself,
    // then every `repeat_every` after that.
    let mut next_over_ceiling_report = ceiling_at;

    loop {
        let writes = signals.active_writes.load(Ordering::Acquire);
        // Index jobs bump `indexing_active`, not `active_writes`, so the
        // drain must wait on both — otherwise a shutdown could proceed
        // while the worker is mid-write.
        let indexing = signals.indexing_active.load(Ordering::Relaxed);
        // The worker pool's own count of claimed jobs still running. This —
        // not the flag — is the authority on whether an index job genuinely
        // exists: it is incremented on claim and decremented only when the
        // job's task finishes, so it cannot outlive the work the way the
        // flag can.
        let in_flight = signals.indexing_in_flight.load(Ordering::Relaxed);
        if writes == 0 && !indexing && in_flight == 0 {
            tracing::info!("no active writes or indexing — shutting down");
            break;
        }

        let elapsed = start.elapsed();

        if writes == 0 && in_flight == 0 {
            // Stuck-flag wait: BOUNDED, deliberately.
            //
            // Only work that is genuinely in flight earns an unbounded wait —
            // it holds the DB write lock on a `spawn_blocking` thread nothing
            // can cancel, and abandoning it is the operator's call. This
            // branch is the opposite case: the flag is set but the worker
            // pool reports ZERO jobs in flight, so there is no running work
            // to wait for, and waiting on the flag forever is a hang, not a
            // safeguard.
            //
            // `indexing_active` is cleared in exactly two places in the worker
            // loop, and with `drained` set (which the daemon's Shutdown handler
            // does before this runs) BOTH are unreachable while the job queue
            // is non-empty: the idle branch is skipped because the drained
            // check `continue`s before a job is ever claimed, and the post-job
            // branch clears only when pending + running + in-flight all reach
            // zero. A server-mode daemon told to shut down with work still
            // queued — the "continuous webhook enqueue" case the Shutdown
            // handler already calls out — would never exit at all.
            //
            // The broadcast below is precisely what unblocks it: the worker
            // loop observes shutdown and breaks. That is the pre-existing,
            // working behaviour for this path, so it is kept intact — and the
            // message says what it costs (read service ends here) rather than
            // letting the daemon do it quietly.
            if elapsed >= ceiling_at {
                tracing::warn!(
                    indexing_active = indexing,
                    indexing_in_flight = in_flight,
                    waited_secs = elapsed.as_secs(),
                    "drain ceiling ({ceiling}s) reached with no in-flight writes \
                     and no index job actually running — `indexing_active` is \
                     set but the worker pool reports zero jobs in flight, so \
                     the flag is stale (work is queued that the drained pool \
                     will not claim). Signalling shutdown so the worker loop \
                     breaks; anything still queued is left for the next start. \
                     NOTE: this closes every listener, so reads stop being \
                     served now"
                );
                break;
            }
        } else if elapsed >= next_over_ceiling_report {
            next_over_ceiling_report = elapsed + repeat_every;
            let pid = std::process::id();
            if writes > 0 {
                tracing::warn!(
                    active_writes = writes,
                    indexing_active = indexing,
                    waited_secs = elapsed.as_secs(),
                    pid,
                    "drain ceiling ({ceiling}s) exceeded — still waiting on {writes} \
                     in-flight write(s){}; the daemon CANNOT abort them and is NOT \
                     shutting down. NEW writes are already being refused with \
                     UNAVAILABLE, so this count only falls. Most reads are still \
                     served, though they can stall for seconds while a write \
                     commits. `embed` takes the write gate AND the write guard, so \
                     it is refused outright; `plan_embed` takes the gate but counts \
                     as a read, so it is not refused — it BLOCKS until the \
                     in-flight write releases the gate. This is \
                     the same drain whether you sent SIGTERM (`nestweaver daemon \
                     stop`) or the Shutdown RPC (`nestweaver daemon restart`); \
                     neither escalates on its own. To end it now — abandoning the \
                     in-flight write, which the graph store may not survive cleanly \
                     (nw-126) — run `nestweaver daemon stop --force` or `kill -9 \
                     {pid}`",
                    // The in-flight counter, not the flag, says whether an index
                    // job is really running alongside the write.
                    if in_flight > 0 {
                        " (an index job is also genuinely in flight)"
                    } else if indexing {
                        " (indexing_active is also set, but no index job is in flight — the flag is stale)"
                    } else {
                        ""
                    },
                );
            } else {
                tracing::warn!(
                    indexing_in_flight = in_flight,
                    waited_secs = elapsed.as_secs(),
                    pid,
                    "drain ceiling ({ceiling}s) exceeded — an index job is genuinely \
                     still running (the worker pool reports {in_flight} job(s) in \
                     flight — its own counter, not the flag); the daemon CANNOT \
                     abort it and is NOT shutting down. NEW writes are already \
                     being refused with UNAVAILABLE, and reads keep being served, \
                     though they can stall while the job's write commits. This is \
                     the same drain whether you sent SIGTERM (`nestweaver daemon \
                     stop`) or the Shutdown RPC (`nestweaver daemon restart`); \
                     neither escalates on its own. If the job is wedged rather \
                     than slow, ending it — abandoning its write, which the graph \
                     store may not survive cleanly (nw-126) — is the operator's \
                     call: `nestweaver daemon stop --force` or `kill -9 {pid}`"
                );
            }
        }

        // Past the ceiling these are noise — the reports above have taken over.
        if elapsed < ceiling_at {
            if !warned_half && elapsed >= half {
                tracing::warn!(
                    active_writes = writes,
                    "drain at 50% of ceiling ({ceiling}s)"
                );
                warned_half = true;
            }
            if !warned_ninety && elapsed >= ninety {
                tracing::warn!(
                    active_writes = writes,
                    "drain at 90% of ceiling ({ceiling}s)"
                );
                warned_ninety = true;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let _ = signals.shutdown_tx.send(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Create a tiny git repo at `dir` with `files` committed, as the `file://`
    /// fetch source for a real worker-pool index job (allowed here by the
    /// `cfg(test)` clone allowance in `bare_clone`).
    fn create_source_repo(dir: &std::path::Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);
        for (path, content) in files {
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
    }

    /// A GENUINELY RUNNING index job past the drain ceiling must not cost read
    /// service. The drain historically consulted only `indexing_active` — a
    /// flag that can outlive the work — so at the ceiling it broadcast shutdown
    /// (closing every listener, ending reads) even while a real worker-pool job
    /// was mid-commit, because it could not tell a live job from a stuck flag.
    ///
    /// This drives a REAL worker-pool index (real `JobQueue`, real bare-clone
    /// fetch, real commit into a real store) and holds the job in flight past
    /// the ceiling by holding the write gate: the worker's commit blocks in
    /// `blocking_lock("worker_commit")`, which is exactly a job that is
    /// running, not a flag that is stuck. Past the ceiling the drain must NOT
    /// have broadcast — the broadcast is what closes the listeners, so its
    /// absence IS "reads are still served". Once the gate is released the job
    /// finishes and the drain must complete and broadcast.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_with_genuinely_running_index_keeps_serving_reads_past_ceiling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[("lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }")],
        );
        let url = format!("file://{}", src.display());

        // A real job queue with one repo to index.
        let queue = crate::jobs::JobQueue::open(&tmp.path().join("jobs.db")).unwrap();
        queue
            .upsert(
                "live-index-repo",
                &url,
                crate::jobs::JobTrigger::Unindexed,
                None,
            )
            .unwrap();
        let queue = Arc::new(Mutex::new(queue));
        let workspace = Arc::new(
            crate::bare_clone::BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap(),
        );
        let store = Arc::new(nestweaver_store::GraphStore::in_memory().unwrap());

        // The drain reads the worker's own status handles — the same Arcs the
        // worker mutates, wired exactly as the daemon wires them.
        let status = crate::worker::IndexingStatus::new();
        let write_gate = crate::write_gate::WriteGate::new();
        let drained = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut worker_shutdown = shutdown_tx.subscribe();
        let signals = DrainSignals {
            active_writes: Arc::new(AtomicU32::new(0)),
            indexing_active: Arc::clone(&status.active),
            indexing_in_flight: Arc::clone(&status.in_flight),
            shutdown_tx,
        };

        let pool = crate::worker::WorkerPool::new(1);
        let worker_queue = Arc::clone(&queue);
        let worker_workspace = Arc::clone(&workspace);
        let worker_store = Arc::clone(&store);
        let worker_status = status.clone();
        let worker_drained = Arc::clone(&drained);
        let worker_gate = write_gate.clone();
        let worker = tokio::spawn(async move {
            pool.run_with_drain(
                worker_queue,
                worker_workspace,
                worker_store,
                crate::worker::fixed_instance_id("test"),
                &mut worker_shutdown,
                Some(worker_status),
                Some(worker_drained),
                Some(worker_gate),
            )
            .await;
        });

        // Hold the write gate so the job's commit phase blocks: the job is
        // genuinely running — claimed, fetched, parsed, and waiting to write —
        // for exactly as long as this lease is held.
        let held = write_gate.lock("test_holds_gate").await;

        // Wait until the worker's commit is actually blocked on the gate —
        // observable proof a job is in flight, not merely a flag set.
        let blocked = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while write_gate.waiting() == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            blocked.is_ok(),
            "the worker must reach its commit phase and block on the write gate"
        );
        assert!(
            status.active.load(Ordering::Relaxed),
            "precondition: the indexing flag is set for a real job"
        );

        // Start the drain with a 1s ceiling, as the daemon's Shutdown handler
        // does (it sets `drained` first, then runs the drain).
        drained.store(true, Ordering::Relaxed);
        let drain = tokio::spawn(run_drain(signals, 1));

        // Well past the ceiling, with the job still genuinely in flight, the
        // drain must NOT have broadcast: the broadcast closes every listener,
        // so its absence is what "reads are still served" means.
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
        assert!(
            !*shutdown_rx.borrow(),
            "a genuinely running index job past the ceiling must not end read \
             service: the drain broadcast (which closes every listener) fired \
             anyway, because the drain cannot tell a live job from a stuck flag"
        );

        // Release the gate: the job commits, the flag clears, and the drain
        // must then complete and broadcast.
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(30), drain)
            .await
            .expect("the drain must complete once the index job finishes")
            .expect("drain task panicked");
        assert!(
            *shutdown_rx.borrow(),
            "once the job really finishes the drain must still shut down cleanly"
        );
        tokio::time::timeout(std::time::Duration::from_secs(30), worker)
            .await
            .expect("the worker must exit after the shutdown broadcast")
            .expect("worker task panicked");
    }
}
