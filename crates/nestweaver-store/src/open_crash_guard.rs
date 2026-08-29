//! nw-285: make a fatal crash inside an lbug database open ATTRIBUTABLE.
//!
//! ## What this is, and what it deliberately is not
//!
//! Opening a `.lbug` file whose on-disk index has been corrupted kills the
//! process with SIGSEGV. The symbolicated stack is entirely vendored C++ —
//! `StorageManager::recover` -> `WALReplayer::replay` ->
//! `Checkpointer::readCheckpoint` -> `NodeTable::deserialize` ->
//! `PrimaryKeyIndex::load` -> `HashIndex` -> `DiskArrayInternal` — below
//! `lbug::main::Database::initMembers`. `lbug` is a pinned crates.io
//! dependency (`= 0.19.1`); the fault is not in Rust this workspace controls,
//! and the real fix (bounds-validating the deserialised index) belongs
//! upstream. See `docs/upstream/lbug-0.19.1-corrupt-index-sigsegv.md`.
//!
//! `open_lbug_with_recovery`'s existing arms all inspect an `Err` that
//! `lbug::Database::new` RETURNED. A SIGSEGV never returns, so none of them
//! can fire. The user gets exit 139 and zero bytes of output —
//! indistinguishable from the process being killed by something else.
//!
//! **This guard does not survive the fault and does not claim to.** It makes
//! the death diagnosable: a signal raised while an open is in flight is
//! reported, by path, with a followable remedy, and the process exits with a
//! normal status code instead of dying on a signal.
//!
//! ## Why not a header / magic-byte check
//!
//! Because it would not work, and would look like it did. The reproduction
//! corrupts bytes at 40%–60% of the file — deep inside the on-disk index
//! region, far past any header. A "validate the first N bytes" guard passes
//! that input, and would therefore pass its own regression test while missing
//! the real fault. Do not re-propose it.
//!
//! ## Why not an out-of-process probe
//!
//! Considered (it was the localisation dossier's recommendation) and rejected
//! on cost and on honesty. It costs a process spawn plus a second full
//! database open on every direct read — paid by every invocation, forever, to
//! catch a rare condition — and it still only covers the paths that remember
//! to call it. This guard costs two `sigaction` calls per open, covers every
//! caller of the single open funnel, and catches ANY fatal signal during the
//! open rather than a particular corruption shape.
//!
//! ## Why not `siglongjmp` out of the handler
//!
//! Unwinding a C++ frame from a signal handler across an FFI boundary is
//! undefined behaviour, and it would leave the buffer manager in an unknown
//! state. The handler here does the two things that are async-signal-safe —
//! `write(2)` a preformatted buffer, then `_exit(2)` — and nothing else.

#[cfg(unix)]
mod imp {
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Preformatted message, built BEFORE the open. A signal handler may not
    /// allocate or format, so the whole diagnostic is assembled here and the
    /// handler only writes bytes it already has.
    static MESSAGE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    /// Raw view of `MESSAGE` for the handler, which cannot take a lock.
    static MESSAGE_PTR: AtomicUsize = AtomicUsize::new(0);
    static MESSAGE_LEN: AtomicUsize = AtomicUsize::new(0);
    /// How many opens are in flight. Zero means a signal is NOT ours and must
    /// be handed back to the default action so ordinary crashes still behave
    /// like ordinary crashes.
    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

    const GUARDED_SIGNALS: [libc::c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];

    extern "C" fn handler(signal: libc::c_int) {
        if IN_FLIGHT.load(Ordering::SeqCst) == 0 {
            // Not ours. Restore the default disposition and re-raise, so a
            // segfault anywhere else still produces the crash report it would
            // have produced without this guard installed.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
            return;
        }
        let ptr = MESSAGE_PTR.load(Ordering::SeqCst) as *const libc::c_void;
        let len = MESSAGE_LEN.load(Ordering::SeqCst);
        if !ptr.is_null() && len > 0 {
            // `write(2)` is async-signal-safe; `println!` is not.
            unsafe {
                let _ = libc::write(libc::STDERR_FILENO, ptr, len);
            }
        }
        // Exit with a NORMAL status. That is the entire point: the caller can
        // tell "the database could not be opened" from "something killed us".
        // `_exit` is async-signal-safe; a normal `exit` would run atexit
        // handlers and C++ destructors from a signal context.
        unsafe { libc::_exit(1) }
    }

    /// Arm the guard for the duration of one open.
    ///
    /// Returns a token that disarms on drop, so an early return or a panic
    /// cannot leave the handler installed with a stale message.
    pub(crate) fn arm(path: &Path) -> Guard {
        let text = format!(
            "\nerror: the database at {} could not be opened: the storage engine \
             crashed while reading it.\n\
             This means the file is corrupt — the on-disk index describes a shape \
             the engine then dereferenced, deep inside the file rather than in its \
             header, so nothing could detect it beforehand.\n\
             Recover it: restore the most recent backup with `nestweaver backup \
             restore <archive>`, or delete this database and re-index the \
             repositories it held.\n\
             Do not keep retrying: this is deterministic, not transient.\n",
            path.display()
        );
        {
            let mut message = MESSAGE.lock().unwrap_or_else(|e| e.into_inner());
            *message = text.into_bytes();
            // Concurrent opens overwrite each other's message; the LAST one
            // armed wins. That is a deliberate simplification — the alternative
            // is serialising every store open behind this guard, and a
            // slightly-wrong path in a crash report is a far smaller cost than
            // that.
            MESSAGE_PTR.store(message.as_ptr() as usize, Ordering::SeqCst);
            MESSAGE_LEN.store(message.len(), Ordering::SeqCst);
        }
        if IN_FLIGHT.fetch_add(1, Ordering::SeqCst) == 0 {
            for signal in GUARDED_SIGNALS {
                unsafe {
                    libc::signal(signal, handler as *const () as libc::sighandler_t);
                }
            }
        }
        Guard
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            if IN_FLIGHT.fetch_sub(1, Ordering::SeqCst) == 1 {
                // Hand the signals back. Leaving the handler installed past
                // the open would swallow the attribution of unrelated crashes
                // — the handler's own zero-in-flight arm covers the race, but
                // restoring is cheaper and clearer than relying on it.
                for signal in GUARDED_SIGNALS {
                    unsafe {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                }
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    pub(crate) struct Guard;

    /// No-op off Unix: the guard is built on `sigaction`/`write`, and the
    /// reported fault is a POSIX signal. Windows would need a vectored
    /// exception handler, which nothing has asked for.
    pub(crate) fn arm(_path: &Path) -> Guard {
        Guard
    }
}

pub(crate) use imp::arm;
