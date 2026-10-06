//! Admission covers one native call, never a connection or result lifetime.
use crate::{CancelReason, StoreError};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Read,
    Exclusive,
}
#[derive(Default)]
struct State {
    readers: usize,
    exclusive: bool,
    exclusive_waiters: usize,
}
#[cfg(test)]
type AdmissionObserver = std::sync::Arc<dyn Fn(Mode, &'static str) + Send + Sync>;

#[derive(Default)]
pub(crate) struct NativeAdmission {
    state: Mutex<State>,
    changed: Condvar,
    connections: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    observer: Mutex<Option<AdmissionObserver>>,
    #[cfg(test)]
    test_deadline: Mutex<Option<Instant>>,
}
#[cfg(test)]
pub(crate) struct TestDeadlineRestore<'a> {
    gate: &'a NativeAdmission,
    previous: Option<Instant>,
}
#[cfg(test)]
impl Drop for TestDeadlineRestore<'_> {
    fn drop(&mut self) {
        *self
            .gate
            .test_deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.previous;
    }
}
pub(crate) struct ConnectionRegistration<'a>(&'a NativeAdmission);
impl Drop for ConnectionRegistration<'_> {
    fn drop(&mut self) {
        self.0
            .connections
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
#[cfg(test)]
pub(crate) struct ObserverRestore<'a> {
    gate: &'a NativeAdmission,
    previous: Option<AdmissionObserver>,
}
#[cfg(test)]
impl Drop for ObserverRestore<'_> {
    fn drop(&mut self) {
        *self.gate.observer.lock().unwrap_or_else(|e| e.into_inner()) = self.previous.take();
    }
}
pub(crate) struct Admission<'a> {
    owner: &'a NativeAdmission,
    mode: Mode,
}
impl NativeAdmission {
    #[cfg(test)]
    pub(crate) fn bound_test_admission(&self, deadline: Instant) -> TestDeadlineRestore<'_> {
        let previous = self
            .test_deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(deadline);
        TestDeadlineRestore {
            gate: self,
            previous,
        }
    }

    pub(crate) fn register_connection(&self) -> ConnectionRegistration<'_> {
        self.connections
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ConnectionRegistration(self)
    }
    pub(crate) fn connection_count(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::Acquire)
    }
    #[cfg(test)]
    pub(crate) fn observe(&self, mode: Mode, operation: &'static str) {
        let observer = self
            .observer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(observer) = observer {
            observer(mode, operation);
        }
    }
    #[cfg(test)]
    pub(crate) fn set_observer(&self, observer: AdmissionObserver) -> ObserverRestore<'_> {
        let previous = self
            .observer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(observer);
        ObserverRestore {
            gate: self,
            previous,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_exclusive_waiter(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exclusive_waiters
            > 0
    }
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.readers == 0 && !state.exclusive
    }

    pub(crate) fn acquire(
        &self,
        mode: Mode,
        read_deadline: Option<Instant>,
    ) -> Result<Admission<'_>, StoreError> {
        self.acquire_inner(mode, read_deadline, true)
    }

    // Destruction may roll back a live write and must always finish. Neither
    // scoped read deadlines nor test-only admission deadlines apply here.
    pub(crate) fn acquire_exclusive_cleanup(&self) -> Admission<'_> {
        self.acquire_inner(Mode::Exclusive, None, false)
            .expect("deadline-free cleanup admission cannot expire")
    }

    fn acquire_inner(
        &self,
        mode: Mode,
        read_deadline: Option<Instant>,
        test_bound: bool,
    ) -> Result<Admission<'_>, StoreError> {
        // Bound deliberately blocked writer workers in integration tests only.
        // Ordinary production writes receive no read deadline.
        #[cfg(test)]
        let read_deadline = if test_bound {
            match (
                *self.test_deadline.lock().unwrap_or_else(|e| e.into_inner()),
                read_deadline,
            ) {
                (Some(test), Some(read)) => Some(test.min(read)),
                (Some(test), None) => Some(test),
                (None, read) => read,
            }
        } else {
            read_deadline
        };
        #[cfg(not(test))]
        let _ = test_bound;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if mode == Mode::Exclusive {
            state.exclusive_waiters += 1;
        }
        loop {
            // Write callers pass None: a read budget must not interrupt commit
            // or checkpoint admission and create an ambiguous write outcome.
            if let Some(deadline) = read_deadline
                && Instant::now() >= deadline
            {
                if mode == Mode::Exclusive {
                    state.exclusive_waiters -= 1;
                    self.changed.notify_all();
                }
                return Err(StoreError::Cancelled(CancelReason::Timeout));
            }
            let admitted = match mode {
                Mode::Read => !state.exclusive && state.exclusive_waiters == 0,
                Mode::Exclusive => !state.exclusive && state.readers == 0,
            };
            if admitted {
                match mode {
                    Mode::Read => state.readers += 1,
                    Mode::Exclusive => {
                        state.exclusive_waiters -= 1;
                        state.exclusive = true;
                    }
                }
                return Ok(Admission { owner: self, mode });
            }
            state = if let Some(deadline) = read_deadline {
                self.changed
                    .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            } else {
                self.changed.wait(state).unwrap_or_else(|e| e.into_inner())
            };
        }
    }
    #[cfg(test)]
    fn call<T>(
        &self,
        mode: Mode,
        deadline: Option<Instant>,
        call: impl FnOnce() -> T,
    ) -> Result<T, StoreError> {
        let _admission = self.acquire(mode, deadline)?;
        Ok(call())
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        match self.mode {
            Mode::Read => state.readers -= 1,
            Mode::Exclusive => state.exclusive = false,
        }
        self.owner.changed.notify_all();
    }
}

// The native analyzer currently calls transaction control read-only. False
// positives in identifiers/strings/comments merely choose conservative
// exclusion; this is not a general Cypher parser or a read-only classifier.
#[cfg(test)]
fn native_execution_mode(native_read_only: bool, source: &str) -> Mode {
    let control = source
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| {
            ["BEGIN", "COMMIT", "ROLLBACK", "CHECKPOINT"]
                .iter()
                .any(|control| word.eq_ignore_ascii_case(control))
        });
    if native_read_only && !control {
        Mode::Read
    } else {
        Mode::Exclusive
    }
}

#[test]
fn admission_allows_overlapping_read_calls_and_excludes_commits() {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    let gate = Arc::new(NativeAdmission::default());
    let (started, reads_started) = mpsc::channel();
    let mut releases = Vec::new();
    let mut readers = Vec::new();
    for _ in 0..2 {
        let (release, wait) = mpsc::channel();
        releases.push(release);
        let gate = Arc::clone(&gate);
        let started = started.clone();
        readers.push(std::thread::spawn(move || {
            gate.call(
                Mode::Read,
                Some(Instant::now() + Duration::from_secs(3)),
                || {
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(3)).is_ok()
                },
            )
        }));
    }
    // A serial-reader regression fails this bounded wait instead of hanging a barrier.
    let overlapping = reads_started.recv_timeout(Duration::from_secs(1)).is_ok()
        && reads_started.recv_timeout(Duration::from_secs(1)).is_ok();
    let writer_gate = Arc::clone(&gate);
    let (entered, writer_entered) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        // The harness bounds its worker even if the contract regresses; actual
        // COMMIT/CHECKPOINT callers supply no read deadline.
        writer_gate.call(
            Mode::Exclusive,
            Some(Instant::now() + Duration::from_secs(3)),
            || entered.send(()).unwrap(),
        )
    });
    let wait_until = Instant::now() + Duration::from_millis(200);
    while gate.state.lock().unwrap().exclusive_waiters == 0 && Instant::now() < wait_until {
        std::thread::yield_now();
    }
    let excluded = writer_entered.try_recv().is_err();
    for release in releases {
        let _ = release.send(());
    }
    let reader_results: Vec<_> = readers.into_iter().map(|reader| reader.join()).collect();
    let writer_result = writer.join();
    assert!(overlapping, "both readers must enter concurrently");
    assert!(excluded, "commit cannot enter while reads execute");
    for result in reader_results {
        assert!(result.unwrap().unwrap());
    }
    assert!(writer_result.unwrap().is_ok());
    assert!(writer_entered.recv_timeout(Duration::from_secs(1)).is_ok());
}

#[test]
fn admission_deadline_is_typed_and_never_enters_native_call() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    let gate = Arc::new(NativeAdmission::default());
    let exclusive = gate.acquire(Mode::Exclusive, None).unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let worker_gate = Arc::clone(&gate);
    let worker_entered = Arc::clone(&entered);
    let worker = std::thread::spawn(move || {
        worker_gate.call(
            Mode::Read,
            Some(Instant::now() + Duration::from_millis(30)),
            || worker_entered.store(true, Ordering::SeqCst),
        )
    });
    let outcome = worker.join().unwrap();
    assert!(matches!(
        outcome,
        Err(StoreError::Cancelled(CancelReason::Timeout))
    ));
    assert!(!entered.load(Ordering::SeqCst));
    drop(exclusive);
    assert!(
        gate.call(
            Mode::Read,
            Some(Instant::now() + std::time::Duration::from_millis(30)),
            || ()
        )
        .is_ok()
    );
}

#[test]
fn returned_rows_and_outer_connection_do_not_hold_admission() {
    let gate = NativeAdmission::default();
    let rows = gate.call(Mode::Read, None, || vec![1, 2, 3]).unwrap();
    // Keep the result while a commit and a nested read finish. No connection-
    // or result-lifetime lock can satisfy this contract.
    assert!(
        gate.call(
            Mode::Exclusive,
            Some(Instant::now() + std::time::Duration::from_millis(30)),
            || ()
        )
        .is_ok()
    );
    assert!(
        gate.call(
            Mode::Read,
            Some(Instant::now() + std::time::Duration::from_millis(30)),
            || ()
        )
        .is_ok()
    );
    assert_eq!(rows, vec![1, 2, 3]);
}

#[test]
fn controls_override_native_read_only_and_unwind_releases_gate() {
    for query in [
        "COMMIT",
        "CHECKPOINT",
        "BEGIN TRANSACTION",
        "ROLLBACK",
        " /* note */ COMMIT;",
    ] {
        assert_eq!(native_execution_mode(true, query), Mode::Exclusive);
    }
    assert_eq!(
        native_execution_mode(true, "MATCH (s:Symbol) RETURN s.uid"),
        Mode::Read
    );
    assert_eq!(
        native_execution_mode(false, "MATCH (s:Symbol) SET s.name = 'new'"),
        Mode::Exclusive
    );
    let gate = NativeAdmission::default();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = gate.call(Mode::Exclusive, None, || panic!("fixture"));
        }))
        .is_err()
    );
    assert!(
        gate.call(
            Mode::Read,
            Some(Instant::now() + std::time::Duration::from_millis(30)),
            || ()
        )
        .is_ok()
    );
}
