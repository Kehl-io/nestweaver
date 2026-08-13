//! The single-writer gate, plus the metadata that makes waiting on it visible.
//!
//! KùzuDB allows one write transaction, so every writer in the process
//! serialises on one mutex. Before A3 that mutex was anonymous: a caller
//! blocked on it could not be counted (`queue_depth` reports index JOBS, not
//! RPCs) and could not be told what was in front of it, so `brain status`
//! reported `queue_depth: 0` while a command sat blocked for ten minutes.
//!
//! This lives in `nestweaver-engine` rather than in the daemon because the
//! daemon is not the only writer: the worker pool ([`crate::worker`]) and the
//! web admin API also take the lock. Homing the type here lets every one of
//! them stamp the holder, so `write_holder` is never empty while the lock is
//! genuinely held.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Log a waiting acquisition once it has been blocked this long. Nothing was
/// emitted for the 13 hours of the production incident; a single line per
/// blocked acquisition is proportionate.
const WRITE_WAIT_LOG_AFTER: Duration = Duration::from_secs(5);

/// The process-wide write lock and its holder/waiter accounting.
///
/// Cheap to clone — every clone shares the same mutex, counter, and stamp.
#[derive(Clone)]
pub struct WriteGate {
    mutex: Arc<tokio::sync::Mutex<()>>,
    /// Callers blocked in `lock`/`blocking_lock`, excluding the holder.
    waiting: Arc<AtomicU32>,
    holder: Arc<std::sync::Mutex<Option<WriteHolder>>>,
}

#[derive(Clone, Copy)]
struct WriteHolder {
    what: &'static str,
    since: Instant,
}

impl std::fmt::Debug for WriteGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteGate")
            .field("waiting", &self.waiting.load(Ordering::Relaxed))
            .field("holder", &self.holder_snapshot().map(|(what, _)| what))
            .finish()
    }
}

impl Default for WriteGate {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII counter for a caller blocked on the gate. A guard rather than a bare
/// increment so a client that disconnects mid-wait (dropping the handler
/// future) still decrements.
struct WaitTicket(Arc<AtomicU32>);

impl WaitTicket {
    fn new(counter: &Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(counter))
    }
}

impl Drop for WaitTicket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Held write lock. Clears the holder stamp on drop, including on cancellation
/// and on unwind.
pub struct WriteLease {
    holder: Arc<std::sync::Mutex<Option<WriteHolder>>>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.holder.lock() {
            *slot = None;
        }
    }
}

impl WriteGate {
    pub fn new() -> Self {
        Self::from_mutex(Arc::new(tokio::sync::Mutex::new(())))
    }

    /// Wrap an existing write mutex. Used by tests that want to hold the raw
    /// mutex and observe the gate.
    pub fn from_mutex(mutex: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self {
            mutex,
            waiting: Arc::new(AtomicU32::new(0)),
            holder: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// The underlying mutex. Only for tests that need to hold the raw lock;
    /// production writers must go through [`Self::lock`] or
    /// [`Self::blocking_lock`] so the holder is stamped.
    pub fn mutex(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.mutex)
    }

    /// Acquire the write lock for `what`, counting the wait and stamping the
    /// holder. This adds accounting, never a timeout: it waits as long as the
    /// previous holder takes.
    pub async fn lock(&self, what: &'static str) -> WriteLease {
        let guard = match Arc::clone(&self.mutex).try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                let ticket = WaitTicket::new(&self.waiting);
                let blocked_on = self.holder_snapshot();
                let waited_since = Instant::now();
                tracing::debug!(
                    what,
                    blocked_on = blocked_on.as_ref().map(|(name, _)| name.as_str()),
                    "writer is waiting for the write lock"
                );
                let guard = Arc::clone(&self.mutex).lock_owned().await;
                Self::log_wait(what, waited_since, blocked_on);
                drop(ticket);
                guard
            }
        };
        self.stamp(what, guard)
    }

    /// [`Self::lock`] for callers that acquire from inside `spawn_blocking` or
    /// a dedicated thread. Same accounting; must not run on a runtime worker
    /// thread.
    pub fn blocking_lock(&self, what: &'static str) -> WriteLease {
        let guard = match Arc::clone(&self.mutex).try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                let ticket = WaitTicket::new(&self.waiting);
                let blocked_on = self.holder_snapshot();
                let waited_since = Instant::now();
                let guard = Arc::clone(&self.mutex).blocking_lock_owned();
                Self::log_wait(what, waited_since, blocked_on);
                drop(ticket);
                guard
            }
        };
        self.stamp(what, guard)
    }

    fn log_wait(what: &'static str, waited_since: Instant, blocked_on: Option<(String, Duration)>) {
        let waited = waited_since.elapsed();
        if waited >= WRITE_WAIT_LOG_AFTER {
            tracing::info!(
                what,
                waited_seconds = waited.as_secs(),
                blocked_on = blocked_on.as_ref().map(|(name, _)| name.as_str()),
                "writer acquired the write lock after waiting"
            );
        }
    }

    fn stamp(&self, what: &'static str, guard: tokio::sync::OwnedMutexGuard<()>) -> WriteLease {
        if let Ok(mut slot) = self.holder.lock() {
            *slot = Some(WriteHolder {
                what,
                since: Instant::now(),
            });
        }
        WriteLease {
            holder: Arc::clone(&self.holder),
            _guard: guard,
        }
    }

    /// Callers blocked on the gate right now, excluding the holder.
    pub fn waiting(&self) -> u32 {
        self.waiting.load(Ordering::Relaxed)
    }

    /// `(what holds it, how long it has held it)`, or `None` when free.
    pub fn holder_snapshot(&self) -> Option<(String, Duration)> {
        self.holder
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .map(|held| (held.what.to_string(), held.since.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_free_gate_reports_no_holder_and_no_waiters() {
        let gate = WriteGate::new();
        assert_eq!(gate.waiting(), 0);
        assert!(gate.holder_snapshot().is_none());
    }

    #[tokio::test]
    async fn a_blocked_caller_is_counted_and_the_holder_is_named() {
        let gate = WriteGate::new();
        let held = gate.lock("embed").await;
        assert_eq!(
            gate.holder_snapshot().map(|(what, _)| what),
            Some("embed".to_string())
        );

        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            let lease = waiter_gate.lock("index_repo").await;
            drop(lease);
        });
        while gate.waiting() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            gate.waiting(),
            1,
            "a blocked writer must not report as zero"
        );

        drop(held);
        waiter.await.expect("waiter completes");
        assert_eq!(gate.waiting(), 0);
        assert!(gate.holder_snapshot().is_none(), "the stamp must clear");
    }

    /// The whole point of homing this in the engine: the worker pool and the
    /// web admin API acquire it too, and a status reader must never see an
    /// empty holder while the lock is genuinely held.
    #[tokio::test]
    async fn a_non_rpc_writer_is_named_just_like_an_rpc_one() {
        let gate = WriteGate::new();
        let worker_gate = gate.clone();
        let held = tokio::task::spawn_blocking(move || worker_gate.blocking_lock("worker_commit"))
            .await
            .expect("blocking acquire");
        assert_eq!(
            gate.holder_snapshot().map(|(what, _)| what),
            Some("worker_commit".to_string()),
        );
        drop(held);
        assert!(gate.holder_snapshot().is_none());
    }

    /// A future dropped while blocked must not leak a phantom waiter — that
    /// would make `write_queue_depth` drift upward forever.
    #[tokio::test]
    async fn a_cancelled_waiter_does_not_leak_the_count() {
        let gate = WriteGate::new();
        let held = gate.lock("embed").await;
        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            let _lease = waiter_gate.lock("index_repo").await;
        });
        while gate.waiting() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        waiter.abort();
        let _ = waiter.await;
        // The abort drops the waiting future; the ticket must have gone with it.
        for _ in 0..100 {
            if gate.waiting() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(gate.waiting(), 0, "a cancelled waiter must be decremented");
        drop(held);
    }
}
