//! Disclose how the previous daemon for a database ended.
//!
//! A daemon that dies on a fatal signal takes the client's connection with it,
//! so the client sees a broken pipe and the next command quietly respawns a
//! daemon that reports itself healthy. Nothing says the storage engine crashed,
//! and nothing says the database may be corrupt. This module records the death
//! so both of those can be said.
//!
//! ## What is written, and when
//!
//! Two files live in the daemon's per-instance state directory:
//!
//! * [`RUNNING_FILE`] is a sentinel holding the daemon's pid. It is written when
//!   the daemon arms and removed whenever `run_server` returns, cleanly or with
//!   an error. A sentinel found by the NEXT daemon therefore means the process
//!   vanished without returning: it was killed.
//! * [`CRASH_FILE`] is an append-only record, one line per unclean exit:
//!   `<signal> <unix seconds> <pid>`. A fatal-signal handler appends a line with
//!   the signal number. A daemon that finds a stale sentinel with no line for
//!   that pid appends one with signal `0`, which means "exited unexpectedly":
//!   SIGKILL and the OOM killer cannot be caught, so nothing is claimed about
//!   the storage engine.
//!
//! ## The clearing rule
//!
//! The record is kept until a daemon shuts down CLEANLY, and that shutdown
//! deletes it. One crash therefore warns for as long as the daemon that
//! replaced the crashed one is running, and stops warning once a daemon has
//! started and stopped without incident. The count is the number of unclean
//! exits since the last clean shutdown.
//!
//! ## The handler
//!
//! It formats into a stack buffer and calls `time`, `getpid`, `write`,
//! `sigaction` and `raise`, all async-signal-safe. It takes no lock and does
//! not allocate. After recording it restores the default action and re-raises,
//! so the process still dies on the signal and the operating system still
//! writes its crash report. Nothing here survives the fault or touches the
//! database.

use std::path::{Path, PathBuf};

/// Sentinel: present while a daemon is running, removed when it returns.
pub const RUNNING_FILE: &str = "daemon.running";
/// Append-only record of unclean exits since the last clean shutdown.
pub const CRASH_FILE: &str = "daemon.crash";

/// The recovery both the client diagnostic and the status warning offer.
pub const RECOVERY: &str = "restore the most recent backup with `nestweaver backup restore \
     <archive>`, or rebuild the database with `nestweaver publication rebuild --config \
     <instance.toml>` (without a config, re-index its repositories into a new database)";

/// The most recent unclean daemon exit, plus how many there have been since
/// the last clean shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncleanExit {
    /// The fatal signal, or `None` when the daemon disappeared without one
    /// being caught (SIGKILL, the OOM killer, power loss).
    pub signal: Option<i32>,
    /// Unix seconds. For a caught signal this is when it was raised; for an
    /// unexplained exit it is when the next daemon noticed.
    pub at: u64,
    /// Pid of the daemon that exited.
    pub pid: u32,
    /// Unclean exits recorded since the last clean shutdown, this one included.
    pub count: usize,
}

impl UncleanExit {
    /// `SIGSEGV`-style name of the fatal signal, when one was caught.
    pub fn signal_name(&self) -> Option<&'static str> {
        self.signal.map(signal_name)
    }

    /// One sentence saying what happened, shared by every surface that reports
    /// it so the CLI, the MCP result and `brain status` cannot drift apart.
    pub fn summary(&self) -> String {
        let times = if self.count == 1 {
            "1 unclean exit".to_string()
        } else {
            format!("{} unclean exits", self.count)
        };
        match self.signal_name() {
            Some(name) => format!(
                "the previous daemon (pid {}) was killed by {name} at {} — the storage \
                 engine crashed, and the database may be corrupt ({times} since the last \
                 clean shutdown)",
                self.pid,
                format_utc(self.at),
            ),
            None => format!(
                "the previous daemon (pid {}) exited unexpectedly without shutting down \
                 (noticed at {}); it was killed from outside, for example by `kill -9` or \
                 the out-of-memory killer, and no storage-engine crash was recorded \
                 ({times} since the last clean shutdown)",
                self.pid,
                format_utc(self.at),
            ),
        }
    }

    /// The message a client reports when its request died with the daemon.
    pub fn client_message(&self, db_path: &Path) -> String {
        let signal = self.signal_name().unwrap_or("a fatal signal");
        format!(
            "the storage engine crashed while answering this request: the daemon for {} \
             was killed by {signal}. The database may be corrupt. Recover it: {RECOVERY}. \
             Do not keep retrying: the same request will crash the daemon again.",
            db_path.display()
        )
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGABRT => "SIGABRT",
        libc::SIGILL => "SIGILL",
        _ => "a fatal signal",
    }
}

/// `YYYY-MM-DD HH:MM:SS UTC`, without pulling a date crate into the store.
fn format_utc(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let rest = unix_seconds % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), valid for the Unix era.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Parse one `<signal> <unix seconds> <pid>` line. A torn or foreign line is
/// skipped rather than guessed at.
fn parse_line(line: &str) -> Option<(i32, u64, u32)> {
    let mut fields = line.split_ascii_whitespace();
    let signal = fields.next()?.parse().ok()?;
    let at = fields.next()?.parse().ok()?;
    let pid = fields.next()?.parse().ok()?;
    Some((signal, at, pid))
}

fn read_lines(dir: &Path) -> Vec<(i32, u64, u32)> {
    std::fs::read_to_string(dir.join(CRASH_FILE))
        .map(|text| text.lines().filter_map(parse_line).collect())
        .unwrap_or_default()
}

/// The unclean exit recorded in `dir`, if any.
pub fn read(dir: &Path) -> Option<UncleanExit> {
    let lines = read_lines(dir);
    let &(signal, at, pid) = lines.last()?;
    Some(UncleanExit {
        signal: (signal != 0).then_some(signal),
        at,
        pid,
        count: lines.len(),
    })
}

/// A crash recorded in `dir` no longer than `window_secs` ago: the evidence a
/// client needs to say its broken connection was the storage engine dying
/// rather than a transport fault. An unexplained exit is deliberately not
/// returned, because nothing ties it to the engine.
pub fn recent_crash(dir: &Path, window_secs: u64) -> Option<UncleanExit> {
    let exit = read(dir)?;
    exit.signal?;
    (now_unix().saturating_sub(exit.at) <= window_secs).then_some(exit)
}

/// The database and state directory a client process is talking to, recorded
/// at connect time so an error funnel far from the connection can still ask
/// whether the daemon crashed.
static CLIENT_WATCH: std::sync::Mutex<Option<(PathBuf, PathBuf)>> = std::sync::Mutex::new(None);

/// How long after a crash a broken connection is still attributed to it. The
/// pipe breaks within milliseconds of the signal; the slack covers a client
/// that retried the connection before giving up.
const CLIENT_CRASH_WINDOW_SECS: u64 = 60;

/// Record which daemon this client process is connected to.
pub fn watch_as_client(db_path: &Path, state_dir: &Path) {
    if let Ok(mut slot) = CLIENT_WATCH.lock() {
        *slot = Some((db_path.to_path_buf(), state_dir.to_path_buf()));
    }
}

/// True when `message` describes a connection that broke, as opposed to an
/// answer the daemon gave. Only a broken connection can be a crashed daemon.
fn is_broken_connection(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "transport error",
        "broken pipe",
        "connection reset",
        "connection refused",
        "stream closed",
        "error reading a body from connection",
        "connection closed",
        "failed to connect to daemon",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// If `error_text` is a broken daemon connection and the daemon this client
/// connected to has just recorded a crash, the message to report instead.
pub fn crash_behind_broken_connection(error_text: &str) -> Option<(PathBuf, UncleanExit)> {
    if !is_broken_connection(error_text) {
        return None;
    }
    let (db_path, dir) = CLIENT_WATCH.lock().ok()?.clone()?;
    let exit = recent_crash(&dir, CLIENT_CRASH_WINDOW_SECS)?;
    Some((db_path, exit))
}

/// An operator is about to kill the daemon on purpose (`daemon stop --force`).
/// Removing the sentinel first is what makes the next daemon stay quiet: the
/// kill is deliberate, not an unexplained disappearance.
pub fn note_deliberate_kill(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(RUNNING_FILE));
}

/// The state directory THIS process armed as a daemon, so `brain status`
/// served by the daemon can report what it found at startup.
static ARMED_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// The unclean exit the running daemon should warn about, if any. `None` in
/// any process that is not an armed daemon.
pub fn armed_report() -> Option<UncleanExit> {
    let dir = ARMED_DIR.lock().ok()?.clone()?;
    read(&dir)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::fd::IntoRawFd;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Pre-opened, append-mode descriptor for [`CRASH_FILE`]. `-1` when no
    /// daemon is armed, which makes the handler a pure pass-through.
    static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

    pub(crate) const FATAL_SIGNALS: [libc::c_int; 4] =
        [libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGILL];

    /// Write `value` in decimal at `buf[at..]`, returning the new length.
    /// Hand-rolled because `format!` allocates and a signal handler may not.
    fn push_decimal(buf: &mut [u8; 64], mut at: usize, mut value: u64) -> usize {
        let mut digits = [0u8; 20];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            buf[at] = digits[count];
            at += 1;
        }
        at
    }

    /// Append one record line. Async-signal-safe: a stack buffer, `time`,
    /// `getpid` and a single `write`.
    pub(crate) fn record_fatal_signal(signal: libc::c_int) {
        let fd = CRASH_FD.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        let mut buf = [0u8; 64];
        let mut len = push_decimal(&mut buf, 0, signal.max(0) as u64);
        buf[len] = b' ';
        len += 1;
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        len = push_decimal(&mut buf, len, now.max(0) as u64);
        buf[len] = b' ';
        len += 1;
        let pid = unsafe { libc::getpid() };
        len = push_decimal(&mut buf, len, pid.max(0) as u64);
        buf[len] = b'\n';
        len += 1;
        unsafe {
            let _ = libc::write(fd, buf.as_ptr() as *const libc::c_void, len);
        }
    }

    fn set_action(signal: libc::c_int, action: libc::sighandler_t) {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = action;
            // Run on the alternate stack when the thread has one, so a stack
            // overflow can still be recorded.
            sa.sa_flags = libc::SA_ONSTACK;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(signal, &sa, std::ptr::null_mut());
        }
    }

    extern "C" fn handler(signal: libc::c_int) {
        record_fatal_signal(signal);
        // Hand the signal back: the process must still die on it, with the
        // crash report it would have produced without this handler.
        set_action(signal, libc::SIG_DFL);
        unsafe {
            libc::raise(signal);
        }
    }

    /// Put `signal` back the way an armed daemon wants it: this module's
    /// handler when a daemon is armed, the default action otherwise. The open
    /// crash guard calls this when it disarms, so borrowing SIGSEGV for the
    /// length of an open does not leave the daemon unrecorded afterwards.
    pub(crate) fn restore_signal(signal: libc::c_int) {
        if CRASH_FD.load(Ordering::SeqCst) >= 0 {
            set_action(signal, handler as *const () as libc::sighandler_t);
        } else {
            set_action(signal, libc::SIG_DFL);
        }
    }

    /// Held by a running daemon. Dropping it removes the sentinel, so a daemon
    /// that returns an error is not later mistaken for one that was killed.
    pub struct Armed {
        dir: PathBuf,
        fd: libc::c_int,
    }

    impl Armed {
        /// The daemon is shutting down cleanly: forget every recorded unclean
        /// exit. This is the clearing rule.
        pub fn clean_shutdown(self) {
            let _ = std::fs::remove_file(self.dir.join(CRASH_FILE));
        }
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            // Disarm only what this guard armed. A process that runs several
            // daemons in turn (the in-process test servers do) must not have
            // one guard's drop disarm its successor.
            if CRASH_FD
                .compare_exchange(self.fd, -1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                for signal in FATAL_SIGNALS {
                    set_action(signal, libc::SIG_DFL);
                }
            }
            unsafe {
                libc::close(self.fd);
            }
            if let Ok(mut slot) = ARMED_DIR.lock()
                && slot.as_deref() == Some(self.dir.as_path())
            {
                *slot = None;
            }
            let _ = std::fs::remove_file(self.dir.join(RUNNING_FILE));
        }
    }

    /// Arm crash recording for a daemon whose state directory is `dir`.
    ///
    /// Call it only while holding the instance lock: a sentinel left in `dir`
    /// is read as "the previous daemon was killed", which is only true when no
    /// other daemon for this instance can be alive.
    pub fn arm(dir: &Path) -> std::io::Result<Armed> {
        use std::io::Write;

        std::fs::create_dir_all(dir)?;
        let running = dir.join(RUNNING_FILE);
        let mut crash_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(CRASH_FILE))?;

        // A sentinel the previous daemon never removed: it did not return. If
        // its handler recorded a signal the line is already there; otherwise
        // say only what is known, that it exited unexpectedly.
        if let Some(previous) = std::fs::read_to_string(&running)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            && !read_lines(dir).iter().any(|&(_, _, pid)| pid == previous)
        {
            writeln!(crash_log, "0 {} {previous}", now_unix())?;
        }
        std::fs::write(&running, format!("{}\n", std::process::id()))?;

        if let Ok(mut slot) = ARMED_DIR.lock() {
            *slot = Some(dir.to_path_buf());
        }
        let fd = crash_log.into_raw_fd();
        CRASH_FD.store(fd, Ordering::SeqCst);
        for signal in FATAL_SIGNALS {
            set_action(signal, handler as *const () as libc::sighandler_t);
        }
        Ok(Armed {
            dir: dir.to_path_buf(),
            fd,
        })
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    /// No-op off Unix: the record is built on POSIX signals.
    pub struct Armed;

    impl Armed {
        pub fn clean_shutdown(self) {}
    }

    pub fn arm(_dir: &Path) -> std::io::Result<Armed> {
        Ok(Armed)
    }
}

pub use imp::{Armed, arm};
#[cfg(unix)]
pub(crate) use imp::{record_fatal_signal, restore_signal};

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// `arm` owns process-wide signal dispositions and statics, so the tests
    /// that call it must not interleave.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_clean_start_and_stop_leaves_nothing_to_report() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let armed = arm(dir.path()).unwrap();
        assert!(dir.path().join(RUNNING_FILE).exists());
        assert_eq!(armed_report(), None);
        armed.clean_shutdown();
        assert!(!dir.path().join(RUNNING_FILE).exists());
        assert_eq!(read(dir.path()), None);
        assert_eq!(armed_report(), None, "a disarmed process reports nothing");
    }

    #[test]
    fn a_recorded_signal_is_reported_as_an_engine_crash() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let armed = arm(dir.path()).unwrap();
        // The handler's own write path, without raising the signal.
        record_fatal_signal(libc::SIGSEGV);
        let exit = read(dir.path()).expect("the handler recorded a line");
        assert_eq!(exit.signal, Some(libc::SIGSEGV));
        assert_eq!(exit.signal_name(), Some("SIGSEGV"));
        assert_eq!(exit.pid, std::process::id());
        assert_eq!(exit.count, 1);
        assert!(exit.at > 1_600_000_000, "a real timestamp: {}", exit.at);
        assert!(exit.summary().contains("storage engine crashed"));
        assert!(recent_crash(dir.path(), 60).is_some());
        // Simulate the death: the sentinel stays, the guard never runs.
        std::mem::forget(armed);

        // The next daemon must not record the same death twice.
        let next = arm(dir.path()).unwrap();
        let exit = armed_report().expect("the replacement daemon reports it");
        assert_eq!(exit.count, 1, "one death, one line");
        assert_eq!(exit.signal, Some(libc::SIGSEGV));
        next.clean_shutdown();
        assert_eq!(read(dir.path()), None, "a clean shutdown clears it");
    }

    #[test]
    fn a_stale_sentinel_without_a_signal_is_an_unexpected_exit_not_a_crash() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        // A daemon that was SIGKILLed: sentinel present, nothing recorded.
        std::fs::write(dir.path().join(RUNNING_FILE), "4242\n").unwrap();

        let armed = arm(dir.path()).unwrap();
        let exit = armed_report().expect("the killed daemon is reported");
        assert_eq!(exit.signal, None);
        assert_eq!(exit.pid, 4242);
        assert!(exit.summary().contains("exited unexpectedly"));
        assert!(
            !exit.summary().contains("engine crashed"),
            "an uncaught kill must not be blamed on the engine: {}",
            exit.summary()
        );
        assert_eq!(
            recent_crash(dir.path(), 60),
            None,
            "a client must not attribute its broken pipe to an engine crash"
        );
        armed.clean_shutdown();
    }

    #[test]
    fn a_deliberate_kill_is_not_reported() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RUNNING_FILE), "4242\n").unwrap();
        note_deliberate_kill(dir.path());
        let armed = arm(dir.path()).unwrap();
        assert_eq!(armed_report(), None);
        armed.clean_shutdown();
    }

    #[test]
    fn an_error_return_removes_the_sentinel_but_keeps_the_record() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CRASH_FILE), "11 1700000000 77\n").unwrap();
        drop(arm(dir.path()).unwrap());
        assert!(!dir.path().join(RUNNING_FILE).exists());
        let exit = read(dir.path()).expect("only a clean shutdown clears the record");
        assert_eq!((exit.signal, exit.pid, exit.count), (Some(11), 77, 1));

        // And the daemon after that does not invent an unexpected exit.
        let armed = arm(dir.path()).unwrap();
        assert_eq!(armed_report().unwrap().count, 1);
        armed.clean_shutdown();
    }

    #[test]
    fn repeated_crashes_are_counted_and_the_latest_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CRASH_FILE),
            "11 1700000000 10\ngarbage\n0 1700000100 11\n10 1700000200 12\n",
        )
        .unwrap();
        let exit = read(dir.path()).unwrap();
        assert_eq!(exit.count, 3, "a torn line is skipped, not counted");
        assert_eq!(exit.signal_name(), Some("SIGBUS"));
        assert_eq!(exit.at, 1_700_000_200);
        assert!(exit.summary().contains("3 unclean exits"));
        assert_eq!(
            recent_crash(dir.path(), 60),
            None,
            "an old crash is not recent"
        );
    }

    #[test]
    fn only_a_broken_connection_is_attributed_to_a_crash() {
        assert!(is_broken_connection(
            "brain_impact RPC failed: transport error: broken pipe"
        ));
        assert!(!is_broken_connection("tool brain_impact: symbol not found"));
    }

    #[test]
    fn timestamps_render_as_utc() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_709_210_096), "2024-02-29 12:34:56 UTC");
    }
}
