//! CPU budget for the parallel indexing phases.
//!
//! A daemon that saturates every core with tree-sitter parses can trip
//! runningboardd's CPU limit on macOS (observed: killed after a 90 s-CPU /
//! 180 s-window violation mid-index). `CpuThrottle` enforces that limit the
//! way it is defined: it tracks the daemon's process CPU time
//! (`getrusage(RUSAGE_SELF)`, all threads summed) over a rolling 180 s window
//! and only sleeps when the window's total would exceed a budget set safely
//! below the kill line.
//!
//! The limit is per PROCESS, so there is one budget per process
//! ([`CpuThrottle::global`]): back-to-back indexes (boot reconcile of several
//! repos, a watcher batch plus a forced index, code then vault) share it, and
//! CPU spent before the throttle is first consulted is billed too. The daemon
//! samples the window every [`SAMPLE_INTERVAL`], so CPU spent between indexes
//! is billed where it happened.
//!
//! nw-812: this replaced a 5 s duty-cycle window that budgeted half of ONE
//! core for the whole multi-threaded process and billed nothing for the first
//! 5 s. A forced index that crossed 5 s was then billed for its entire
//! 11-core burst, and every worker slept 250 ms per remaining file — a stall
//! proportional to the file count, landing on the type-env build.
//!
//! Behaviour now:
//! - A burst that stays under the budget (a forced index of a large repo is
//!   ~45–60 s of CPU) never sleeps, however many cores it uses.
//! - Sustained demand is held at the budget per rolling window.
//! - Sleeping is time-based: a worker over budget waits until enough old CPU
//!   rolls out of the window, so the stall after a burst is bounded by the
//!   window and does not grow with the number of files left (all workers
//!   wait out the same debt concurrently, instead of each paying a fixed
//!   sleep per file).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Environment variable selecting the budget, as the average percentage of
/// ONE core the daemon process may use over the rolling 180 s window (the
/// same unit macOS reports for its own limit: 90 s / 180 s is 50%). `0` or
/// `>= 100` disables the throttle; values above [`MAX_PERCENT`] fall back to
/// the default.
#[cfg_attr(test, allow(dead_code))] // unit tests use an inert global
const ENV_VAR: &str = "NESTWEAVER_INDEX_CPU_PERCENT";

/// Default budget: 40% of one core over 180 s = 72 s of CPU per window,
/// 20% under the 90 s kill line and enough that a forced index of a large
/// repo (an 11-core burst of a few seconds) runs unthrottled.
const DEFAULT_PERCENT: u32 = 40;

/// Ceiling for configured values: 40% = 72 s per window, the same as the
/// default. The write phase after the gate is not throttled and adds up to
/// [`WRITE_RESERVE`] on top of the budget, so anything higher could carry the
/// window past the 90 s kill line. Values above it — notably the old default
/// `50` that existing launchd plists bake — are treated as unset (the
/// default), not as "as much as allowed".
const MAX_PERCENT: u32 = 40;

/// CPU kept free for the write/epilogue phase, which runs under the write
/// gate and therefore is never throttled (~10–15 s for a large repo). Before
/// taking the gate an index waits until the window is at most
/// `budget - WRITE_RESERVE`; see [`CpuThrottle::reserve_write_headroom`].
const WRITE_RESERVE: Duration = Duration::from_secs(20);

/// The rolling window runningboardd measures CPU over.
const WINDOW: Duration = Duration::from_secs(180);

/// Bucket width for the rolling window.
const SUB_WINDOW: Duration = Duration::from_secs(10);

/// The most one [`CpuThrottle::check`] call will ever wait: the longest any
/// debt can take to roll out of the window. Waiting longer could only mean
/// CPU from outside the throttled work keeps refilling the window, and then
/// the worker proceeds rather than block indefinitely.
const MAX_WAIT_PER_CHECK: Duration = Duration::from_secs(190);

/// How often the daemon calls [`CpuThrottle::sample`]: at most one
/// sub-window, so a gap between measurements never spans more than one.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Upper bound on a single sleep slice, so a waiting worker re-measures and
/// notices cancellation promptly.
const MAX_SLEEP: Duration = Duration::from_millis(250);

/// The process-wide throttle. See [`CpuThrottle::global`].
static THROTTLE: OnceLock<CpuThrottle> = OnceLock::new();

/// Shared rolling-window CPU budget. Interior mutability (a `Mutex` over the
/// window) lets one `&CpuThrottle` be captured by every rayon worker.
pub struct CpuThrottle {
    /// CPU time allowed per rolling [`WINDOW`]; `None` disables the throttle.
    target: Option<Duration>,
    wall_clock: Box<dyn Fn() -> Duration + Send + Sync>,
    cpu_clock: Box<dyn Fn() -> Duration + Send + Sync>,
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
    window: Mutex<Window>,
    /// Set by [`interrupt_waits`](Self::interrupt_waits): every current and
    /// future wait returns at once.
    interrupted: AtomicBool,
    /// nw-812 test seam: how many times [`check`](Self::check) ran.
    #[cfg(test)]
    checks: std::sync::atomic::AtomicUsize,
}

/// CPU consumed per [`SUB_WINDOW`] bucket.
struct Window {
    /// Wall time of the last measurement.
    last_wall: Duration,
    /// Process CPU time at the last measurement. Starts at ZERO: CPU the
    /// process used before the throttle existed is billed, not forgiven.
    last_cpu: Duration,
    /// `(bucket index, CPU billed to it)`, oldest first. Bucket `i` covers
    /// wall time `[i * SUB_WINDOW, (i + 1) * SUB_WINDOW)`.
    buckets: VecDeque<(u64, Duration)>,
    /// Wall time of the last "budget spent" debug line (rate limit).
    last_log: Option<Duration>,
}

impl Window {
    /// Bill CPU used since the last measurement and drop buckets that have
    /// rolled entirely out of the window.
    ///
    /// CPU used across a gap between measurements is spread evenly over the
    /// gap's buckets (only the part still inside the window counts), so a
    /// throttle first consulted long after boot does not bill hours of
    /// background CPU as one burst. A gap of zero bills the current bucket.
    /// A bucket is kept while any part of it is inside the window, so the
    /// measured total covers up to WINDOW + SUB_WINDOW — conservative.
    fn update(&mut self, now_wall: Duration, now_cpu: Duration) {
        let used = now_cpu.saturating_sub(self.last_cpu);
        self.last_cpu = self.last_cpu.max(now_cpu);
        let from = self.last_wall.min(now_wall);
        self.last_wall = now_wall;
        let gap = now_wall - from;
        let current = bucket_index(now_wall);
        if gap.is_zero() || used.is_zero() {
            self.bill(current, used);
        } else {
            // Only buckets that can still be inside the window get a share.
            let horizon = now_wall.saturating_sub(WINDOW + SUB_WINDOW);
            let first = bucket_index(from.max(horizon));
            let rate = used.as_secs_f64() / gap.as_secs_f64();
            for index in first..=current {
                let start = bucket_start(index).max(from);
                let end = bucket_start(index + 1).min(now_wall);
                if end > start {
                    self.bill(index, (end - start).mul_f64(rate));
                }
            }
        }
        while let Some(&(index, _)) = self.buckets.front() {
            if bucket_expiry(index) <= now_wall {
                self.buckets.pop_front();
            } else {
                break;
            }
        }
    }

    fn bill(&mut self, index: u64, cpu: Duration) {
        match self.buckets.back_mut() {
            Some((last, total)) if *last == index => *total += cpu,
            _ => self.buckets.push_back((index, cpu)),
        }
    }

    fn total(&self) -> Duration {
        self.buckets.iter().map(|(_, cpu)| *cpu).sum()
    }

    /// Wall time until enough of the oldest buckets expire to bring the
    /// window back within `budget`.
    fn wait_until_within(&self, budget: Duration, now_wall: Duration) -> Duration {
        let mut remaining = self.total();
        for &(index, cpu) in &self.buckets {
            if remaining <= budget {
                break;
            }
            remaining = remaining.saturating_sub(cpu);
            if remaining <= budget {
                return bucket_expiry(index).saturating_sub(now_wall);
            }
        }
        Duration::ZERO
    }
}

fn bucket_index(wall: Duration) -> u64 {
    (wall.as_nanos() / SUB_WINDOW.as_nanos()) as u64
}

fn bucket_start(index: u64) -> Duration {
    SUB_WINDOW * u32::try_from(index).unwrap_or(u32::MAX)
}

/// Wall time at which bucket `index` has fully left the rolling window.
fn bucket_expiry(index: u64) -> Duration {
    bucket_start(index + 1) + WINDOW
}

impl CpuThrottle {
    /// The process-wide throttle, built from `NESTWEAVER_INDEX_CPU_PERCENT`
    /// on first use (the env is read once). The daemon touches it at boot so
    /// its wall clock starts with the process.
    pub fn global() -> &'static CpuThrottle {
        #[cfg(test)]
        if let Some(installed) = test_seam::installed() {
            return installed;
        }
        // Unit tests: the whole test binary is one process running thousands
        // of indexes in parallel, so a real process-wide budget would make
        // them wait on each other. Throttle behaviour is tested on rigs.
        #[cfg(test)]
        return THROTTLE.get_or_init(|| Self::new(None));
        #[cfg(not(test))]
        THROTTLE.get_or_init(Self::from_env)
    }

    /// Build from `NESTWEAVER_INDEX_CPU_PERCENT` (default 40; `0` or
    /// `>= 100` produces an inert throttle whose [`check`](Self::check)
    /// never sleeps).
    #[cfg_attr(test, allow(dead_code))] // unit tests use an inert global
    fn from_env() -> Self {
        Self::from_env_value(std::env::var(ENV_VAR).ok().as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Self {
        let percent = value
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_PERCENT);
        let percent = if percent > MAX_PERCENT && percent < 100 {
            DEFAULT_PERCENT
        } else {
            percent
        };
        let target =
            (percent > 0 && percent < 100).then(|| WINDOW.mul_f64(f64::from(percent) / 100.0));
        Self::new(target)
    }

    fn new(target: Option<Duration>) -> Self {
        let start = std::time::Instant::now();
        Self::with_clocks(
            target,
            Box::new(move || start.elapsed()),
            Box::new(process_cpu_time),
            Box::new(std::thread::sleep),
        )
    }

    fn with_clocks(
        target: Option<Duration>,
        wall_clock: Box<dyn Fn() -> Duration + Send + Sync>,
        cpu_clock: Box<dyn Fn() -> Duration + Send + Sync>,
        sleep: Box<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        let window = Window {
            last_wall: wall_clock(),
            last_cpu: Duration::ZERO,
            buckets: VecDeque::new(),
            last_log: None,
        };
        Self {
            target,
            wall_clock,
            cpu_clock,
            sleep,
            window: Mutex::new(window),
            interrupted: AtomicBool::new(false),
            #[cfg(test)]
            checks: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Called from parallel workers before a unit of CPU-bound work. Cheap
    /// when under budget and entirely inert when disabled.
    pub fn check(&self) {
        self.check_with_cancel(None);
    }

    /// [`check`](Self::check) that stops waiting as soon as `cancel` is set.
    pub fn check_with_cancel(&self, cancel: Option<&AtomicBool>) {
        #[cfg(test)]
        self.checks.fetch_add(1, Ordering::Relaxed);
        let Some(budget) = self.target else { return };
        self.wait_within(budget, cancel);
    }

    /// Called once before an index takes its write gate (never under it):
    /// waits until the window is at most `budget - WRITE_RESERVE`, so the
    /// unthrottled write phase cannot carry the window past the budget by
    /// more than the reserve. Stops waiting as soon as `cancel` is set.
    pub fn reserve_write_headroom(&self, cancel: Option<&AtomicBool>) {
        #[cfg(test)]
        self.checks.fetch_add(1, Ordering::Relaxed);
        let Some(budget) = self.target else { return };
        self.wait_within(budget.saturating_sub(WRITE_RESERVE), cancel);
    }

    /// End every current and future wait (sticky). The daemon calls this
    /// when it begins shutting down: paths without a job cancel token (the
    /// vault parse) would otherwise hold the drain for up to
    /// [`MAX_WAIT_PER_CHECK`]. Work then runs unthrottled to its end, which
    /// on a draining daemon is a short, bounded tail.
    pub fn interrupt_waits(&self) {
        self.interrupted.store(true, Ordering::Relaxed);
    }

    /// Wait (in [`MAX_SLEEP`] slices, at most [`MAX_WAIT_PER_CHECK`]) until
    /// the window's total is at most `limit`.
    fn wait_within(&self, limit: Duration, cancel: Option<&AtomicBool>) {
        let mut slept = Duration::ZERO;
        while slept < MAX_WAIT_PER_CHECK {
            if self.interrupted.load(Ordering::Relaxed)
                || cancel.is_some_and(|c| c.load(Ordering::Relaxed))
            {
                return;
            }
            let wait = {
                // A poisoned lock only means another worker panicked
                // mid-check; the window is still usable, so recover.
                let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
                // Read both clocks under the lock, so measurements reach
                // `update` in order and no worker bills a stale sample.
                let now_wall = (self.wall_clock)();
                let now_cpu = (self.cpu_clock)();
                window.update(now_wall, now_cpu);
                let total = window.total();
                if total <= limit {
                    return;
                }
                // Rate-limited: one line per sub-window, however many
                // workers are waiting.
                if window
                    .last_log
                    .is_none_or(|at| now_wall.saturating_sub(at) >= SUB_WINDOW)
                {
                    window.last_log = Some(now_wall);
                    tracing::debug!(
                        window_total_secs = total.as_secs_f64(),
                        limit_secs = limit.as_secs_f64(),
                        budget_secs = self.target.unwrap_or_default().as_secs_f64(),
                        "index CPU budget for the rolling 180 s window spent; waiting"
                    );
                }
                window.wait_until_within(limit, now_wall)
            };
            let nap = wait.min(MAX_SLEEP).min(MAX_WAIT_PER_CHECK - slept);
            if nap.is_zero() {
                return;
            }
            (self.sleep)(nap);
            slept += nap;
        }
    }

    /// Bill CPU used since the last measurement to the current bucket,
    /// without waiting. The daemon calls this every [`SAMPLE_INTERVAL`]: CPU
    /// spent between indexes (embedding, watcher batches, reconcilers,
    /// queries) is then billed where it happened. Without it, a long gap's CPU
    /// would be spread evenly over the gap and a burst at its end under-billed.
    pub fn sample(&self) {
        let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
        let now_wall = (self.wall_clock)();
        let now_cpu = (self.cpu_clock)();
        window.update(now_wall, now_cpu);
    }

    /// The window's current total (tests).
    #[cfg(test)]
    fn window_total(&self) -> Duration {
        let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
        window.update((self.wall_clock)(), (self.cpu_clock)());
        window.total()
    }
}

/// nw-812 test seam. A test installs its own throttle on its thread, and
/// [`CpuThrottle::global`] returns it to code running on that thread (the
/// index entry points, which hand the reference to their parse workers).
/// Checks are counted per installed throttle, so parallel tests indexing
/// through the real global never disturb the count.
#[cfg(test)]
pub(crate) mod test_seam {
    use super::CpuThrottle;
    use std::cell::Cell;
    use std::sync::atomic::Ordering;

    thread_local! {
        static INSTALLED: Cell<Option<&'static CpuThrottle>> = const { Cell::new(None) };
    }

    /// Install a disabled (never-sleeping) counting throttle for this thread.
    pub(crate) fn install_counting_throttle() -> &'static CpuThrottle {
        let throttle: &'static CpuThrottle = Box::leak(Box::new(CpuThrottle::new(None)));
        INSTALLED.with(|cell| cell.set(Some(throttle)));
        throttle
    }

    pub(super) fn installed() -> Option<&'static CpuThrottle> {
        INSTALLED.with(Cell::get)
    }

    /// Checks made so far by `throttle`.
    pub(crate) fn checks(throttle: &CpuThrottle) -> usize {
        throttle.checks.load(Ordering::SeqCst)
    }
}

/// Process CPU time (user + system, all threads) via `getrusage`. Returns
/// zero on error or non-unix platforms, which reads as "no CPU used" and
/// therefore never triggers a sleep.
#[cfg(unix)]
fn process_cpu_time() -> Duration {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return Duration::ZERO;
    }
    let to_duration = |tv: libc::timeval| {
        Duration::new(
            u64::try_from(tv.tv_sec).unwrap_or(0),
            u32::try_from(tv.tv_usec).unwrap_or(0) * 1000,
        )
    };
    to_duration(usage.ru_utime) + to_duration(usage.ru_stime)
}

#[cfg(not(unix))]
fn process_cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Injected clocks the test advances by hand, plus a sink recording
    /// every requested sleep.
    struct Rig {
        wall: Arc<StdMutex<Duration>>,
        cpu: Arc<StdMutex<Duration>>,
        sleeps: Arc<StdMutex<Vec<Duration>>>,
        throttle: CpuThrottle,
    }

    fn rig(target: Option<Duration>) -> Rig {
        let wall = Arc::new(StdMutex::new(Duration::ZERO));
        let cpu = Arc::new(StdMutex::new(Duration::ZERO));
        let sleeps = Arc::new(StdMutex::new(Vec::new()));
        let throttle = {
            let wall = Arc::clone(&wall);
            let cpu = Arc::clone(&cpu);
            let sleeps = Arc::clone(&sleeps);
            CpuThrottle::with_clocks(
                target,
                Box::new(move || *wall.lock().unwrap_or_else(|p| p.into_inner())),
                Box::new(move || *cpu.lock().unwrap_or_else(|p| p.into_inner())),
                Box::new(move |d| sleeps.lock().unwrap_or_else(|p| p.into_inner()).push(d)),
            )
        };
        Rig {
            wall,
            cpu,
            sleeps,
            throttle,
        }
    }

    /// A fresh throttle over the same injected clocks as `other`.
    fn rig_like(other: &Rig, target: Option<Duration>) -> Rig {
        let wall = Arc::clone(&other.wall);
        let cpu = Arc::clone(&other.cpu);
        let sleeps = Arc::new(StdMutex::new(Vec::new()));
        let throttle = {
            let wall = Arc::clone(&wall);
            let cpu = Arc::clone(&cpu);
            let sleeps = Arc::clone(&sleeps);
            CpuThrottle::with_clocks(
                target,
                Box::new(move || *wall.lock().unwrap_or_else(|p| p.into_inner())),
                Box::new(move || *cpu.lock().unwrap_or_else(|p| p.into_inner())),
                Box::new(move |d| sleeps.lock().unwrap_or_else(|p| p.into_inner()).push(d)),
            )
        };
        Rig {
            wall,
            cpu,
            sleeps,
            throttle,
        }
    }

    impl Rig {
        fn advance(&self, wall: Duration, cpu: Duration) {
            *self.wall.lock().unwrap_or_else(|p| p.into_inner()) += wall;
            *self.cpu.lock().unwrap_or_else(|p| p.into_inner()) += cpu;
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    fn secs(s: u64) -> Option<Duration> {
        Some(Duration::from_secs(s))
    }

    #[test]
    fn env_default_is_seventy_two_seconds_per_window() {
        let t = CpuThrottle::from_env_value(None);
        assert_eq!(t.target, secs(72));
    }

    #[test]
    fn env_unparsable_falls_back_to_default() {
        let t = CpuThrottle::from_env_value(Some("garbage"));
        assert_eq!(t.target, secs(72));
    }

    #[test]
    fn env_zero_and_hundred_plus_disable() {
        assert_eq!(CpuThrottle::from_env_value(Some("0")).target, None);
        assert_eq!(CpuThrottle::from_env_value(Some("100")).target, None);
        assert_eq!(CpuThrottle::from_env_value(Some("800")).target, None);
    }

    #[test]
    fn env_partial_percent_is_honored() {
        // 25% of one core over 180 s = 45 s of CPU per window.
        let t = CpuThrottle::from_env_value(Some("25"));
        assert_eq!(t.target, secs(45));
    }

    #[test]
    fn legacy_values_over_the_max_map_to_the_default() {
        // Existing plists bake the old default `50`, which is exactly
        // runningboardd's 90 s / 180 s kill line. Anything over the max is
        // treated as unset, not as "as much as allowed".
        let default = CpuThrottle::from_env_value(None).target;
        for legacy in ["50", "41", "45", "99"] {
            assert_eq!(
                CpuThrottle::from_env_value(Some(legacy)).target,
                default,
                "{legacy} must map to the default"
            );
        }
        assert_eq!(CpuThrottle::from_env_value(Some("40")).target, secs(72));
    }

    #[test]
    fn env_value_is_trimmed() {
        let t = CpuThrottle::from_env_value(Some(" 30 "));
        assert_eq!(t.target, secs(54));
    }

    #[test]
    fn disabled_throttle_never_sleeps() {
        let rig = rig(None);
        for _ in 0..10 {
            rig.advance(Duration::from_secs(10), Duration::from_secs(60));
            rig.throttle.check();
        }
        assert!(rig.sleeps().is_empty());
    }

    #[test]
    fn under_budget_does_not_sleep() {
        let rig = rig(secs(72));
        rig.advance(Duration::from_secs(5), Duration::from_secs(70));
        rig.throttle.check();
        assert!(rig.sleeps().is_empty());
    }

    #[test]
    fn over_budget_wait_is_sliced_and_bounded_per_check() {
        // The injected sleep does not advance the wall clock here, so the
        // window never recovers: one check must still give up after the
        // longest any debt could take to clear, in MAX_SLEEP slices.
        let rig = rig(secs(72));
        rig.advance(Duration::from_secs(5), Duration::from_secs(80));
        rig.throttle.check();
        let sleeps = rig.sleeps();
        assert!(sleeps.iter().all(|d| *d <= MAX_SLEEP));
        assert_eq!(sleeps.iter().sum::<Duration>(), MAX_WAIT_PER_CHECK);
    }

    #[test]
    fn cancellation_stops_waiting() {
        let rig = rig(secs(72));
        rig.advance(Duration::from_secs(5), Duration::from_secs(80));
        let cancel = AtomicBool::new(true);
        rig.throttle.check_with_cancel(Some(&cancel));
        assert!(rig.sleeps().is_empty());
    }

    #[test]
    fn usage_rolls_out_of_the_window() {
        let rig = rig(secs(72));
        rig.advance(Duration::from_secs(5), Duration::from_secs(70));
        rig.throttle.check();
        // 190 s later the first bucket has fully expired: a fresh 70 s burst
        // is judged on its own.
        rig.advance(Duration::from_secs(190), Duration::from_secs(70));
        rig.throttle.check();
        assert!(rig.sleeps().is_empty());
    }

    // ── nw-812: rolling-window budget ────────────────────────────────────

    /// Rig whose injected sleep advances the wall clock, so a simulated
    /// worker that sleeps actually lets time pass.
    fn sim_rig() -> Rig {
        let wall = Arc::new(StdMutex::new(Duration::ZERO));
        let cpu = Arc::new(StdMutex::new(Duration::ZERO));
        let sleeps = Arc::new(StdMutex::new(Vec::new()));
        let throttle = {
            let wall_c = Arc::clone(&wall);
            let wall_s = Arc::clone(&wall);
            let cpu = Arc::clone(&cpu);
            let sleeps = Arc::clone(&sleeps);
            CpuThrottle::with_clocks(
                CpuThrottle::from_env_value(None).target,
                Box::new(move || *wall_c.lock().unwrap_or_else(|p| p.into_inner())),
                Box::new(move || *cpu.lock().unwrap_or_else(|p| p.into_inner())),
                Box::new(move |d| {
                    *wall_s.lock().unwrap_or_else(|p| p.into_inner()) += d;
                    sleeps.lock().unwrap_or_else(|p| p.into_inner()).push(d);
                }),
            )
        };
        Rig {
            wall,
            cpu,
            sleeps,
            throttle,
        }
    }

    impl Rig {
        fn now(&self) -> (Duration, Duration) {
            (
                *self.wall.lock().unwrap_or_else(|p| p.into_inner()),
                *self.cpu.lock().unwrap_or_else(|p| p.into_inner()),
            )
        }

        fn total_sleep(&self) -> Duration {
            self.sleeps().iter().sum()
        }
    }

    /// Largest CPU consumed in any `window` of wall time across a trace of
    /// monotonically increasing `(wall, cpu)` samples.
    fn max_rolling_cpu(trace: &[(Duration, Duration)], window: Duration) -> Duration {
        let mut max = Duration::ZERO;
        let mut lo = 0;
        for hi in 0..trace.len() {
            while trace[hi].0.saturating_sub(trace[lo].0) > window {
                lo += 1;
            }
            max = max.max(trace[hi].1.saturating_sub(trace[lo].1));
        }
        max
    }

    /// The default budget is 72 s of CPU per rolling 180 s window (40% of one
    /// core averaged), 20% under runningboardd's 90 s / 180 s kill line.
    const DEFAULT_BUDGET_SECS: f64 = 72.0;

    #[test]
    fn eleven_core_burst_of_six_seconds_never_sleeps() {
        // A forced index of a large repo: 11 parse workers flat out for 6 s
        // (66 s of CPU), checking before every 0.1 s-CPU file. This is under
        // the rolling budget, so no worker may ever sleep — the old 5 s
        // duty-cycle window billed it as 16x over a half-core target.
        let rig = sim_rig();
        for _ in 0..660 {
            rig.throttle.check();
            rig.advance(
                Duration::from_secs_f64(6.0 / 660.0),
                Duration::from_millis(100),
            );
        }
        assert!(
            rig.sleeps().is_empty(),
            "a 66 s-CPU burst must not sleep; slept {:?}",
            rig.total_sleep()
        );
    }

    #[test]
    fn sustained_over_budget_use_is_held_at_the_rolling_cap() {
        // 11 cores of demand for 20 simulated minutes (a long vault index).
        let rig = sim_rig();
        let mut trace = vec![rig.now()];
        while rig.now().0 < Duration::from_secs(1200) {
            rig.throttle.check();
            rig.advance(Duration::from_millis(10), Duration::from_millis(110));
            trace.push(rig.now());
        }
        let peak = max_rolling_cpu(&trace, Duration::from_secs(180));
        assert!(
            peak.as_secs_f64() <= DEFAULT_BUDGET_SECS + 1.0,
            "rolling 180 s CPU peaked at {peak:?}, over the {DEFAULT_BUDGET_SECS} s budget"
        );
        // ...while still making real progress (not starved far below cap).
        let total = rig.now().1.as_secs_f64();
        assert!(
            total >= DEFAULT_BUDGET_SECS * 5.0,
            "throughput collapsed: only {total:.1} s CPU in 1200 s"
        );
    }

    fn sleep_after_late_billed_burst(remaining_files: usize) -> (Duration, Duration) {
        let rig = sim_rig();
        // A burst billed late: 11 cores for 20 s with no check in between.
        rig.advance(Duration::from_secs(20), Duration::from_secs(220));
        let mut max_single_check = Duration::ZERO;
        for _ in 0..remaining_files {
            let before = rig.total_sleep();
            rig.throttle.check();
            max_single_check = max_single_check.max(rig.total_sleep() - before);
            // Cheap remaining files (e.g. type-env builds): ~no CPU.
            rig.advance(Duration::from_millis(1), Duration::ZERO);
        }
        (rig.total_sleep(), max_single_check)
    }

    #[test]
    fn sleep_after_a_burst_is_bounded_and_independent_of_remaining_files() {
        let (few, few_max) = sleep_after_late_billed_burst(2_000);
        let (many, many_max) = sleep_after_late_billed_burst(20_000);
        assert_eq!(
            few, many,
            "post-burst sleep must not scale with the number of remaining files"
        );
        // Debt clears as the burst rolls out of the window: never more than
        // one window plus one sub-window of wall time, whatever the burst.
        assert!(
            many <= Duration::from_secs(190),
            "post-burst sleep {many:?} exceeds one window"
        );
        // The whole debt is repaid by the first check that sees it; later
        // checks proceed without paying anything per file.
        assert_eq!(few_max, few);
        assert_eq!(many_max, many);
    }

    // ── nw-812 review C1: one process-wide budget ────────────────────────

    #[test]
    fn cpu_spent_before_the_throttle_existed_is_billed() {
        // runningboardd counts the PROCESS, so CPU burned before this
        // throttle was first consulted (an earlier index, boot work) must
        // count against the window rather than being forgiven at creation.
        let rig = rig(secs(72));
        rig.advance(Duration::ZERO, Duration::from_secs(80));
        // Rebuild the throttle now that 80 s of CPU is already on the clock.
        let rig = rig_like(&rig, secs(72));
        rig.throttle.check();
        assert!(
            !rig.sleeps().is_empty(),
            "80 s of prior process CPU against a 72 s budget must wait"
        );
    }

    #[test]
    fn back_to_back_indexes_share_one_budget() {
        let rig = sim_rig();
        let burst = |rig: &Rig| {
            let before = rig.total_sleep();
            for _ in 0..660 {
                rig.throttle.check();
                rig.advance(
                    Duration::from_secs_f64(6.0 / 660.0),
                    Duration::from_millis(100),
                );
            }
            rig.total_sleep() - before
        };
        assert_eq!(burst(&rig), Duration::ZERO, "the first index runs free");
        assert!(
            burst(&rig) > Duration::ZERO,
            "a second index right behind it must wait for the shared window"
        );
    }

    #[test]
    fn global_throttle_is_one_process_wide_instance() {
        assert!(std::ptr::eq(CpuThrottle::global(), CpuThrottle::global()));
    }

    // ── nw-812 review C2: headroom for the unthrottled write phase ───────

    #[test]
    fn write_phase_starts_with_headroom() {
        // Parse + type-env used 60 s: under the 72 s budget, so no worker
        // waited. The write/epilogue phase is not throttled and adds ~10–15 s,
        // so before the gate the window must come back to budget − reserve.
        let rig = sim_rig();
        for _ in 0..600 {
            rig.throttle.check();
            rig.advance(Duration::from_millis(10), Duration::from_millis(100));
        }
        assert!(rig.sleeps().is_empty(), "60 s is under budget");
        rig.throttle.reserve_write_headroom(None);
        assert!(!rig.sleeps().is_empty(), "60 s leaves no write reserve");
        assert!(
            rig.throttle.window_total() <= Duration::from_secs(72) - WRITE_RESERVE,
            "the write phase must start with {WRITE_RESERVE:?} of headroom, window holds {:?}",
            rig.throttle.window_total()
        );
    }

    #[test]
    fn write_headroom_needs_no_wait_when_already_available() {
        let rig = sim_rig();
        rig.advance(Duration::from_secs(5), Duration::from_secs(40));
        rig.throttle.reserve_write_headroom(None);
        assert!(rig.sleeps().is_empty());
    }

    // ── nw-812 review I2: waits are interruptible without a cancel token ─

    #[test]
    fn interrupt_waits_releases_a_waiting_worker() {
        // The vault parse has no job cancel token; a daemon shutdown must
        // still be able to end a throttle wait instead of blocking ~190 s.
        let rig = rig(secs(72));
        rig.advance(Duration::from_secs(5), Duration::from_secs(80));
        rig.throttle.interrupt_waits();
        rig.throttle.check();
        rig.throttle.reserve_write_headroom(None);
        assert!(rig.sleeps().is_empty(), "slept {:?}", rig.sleeps().len());
    }

    #[test]
    fn interrupt_waits_ends_a_wait_already_in_progress() {
        let throttle = Arc::new(StdMutex::new(None::<Arc<CpuThrottle>>));
        let slices = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let t = {
            let throttle = Arc::clone(&throttle);
            let slices = Arc::clone(&slices);
            Arc::new(CpuThrottle::with_clocks(
                secs(72),
                Box::new(|| Duration::from_secs(5)),
                Box::new(|| Duration::from_secs(80)),
                Box::new(move |_| {
                    // Shutdown arrives during the first sleep slice.
                    slices.fetch_add(1, Ordering::SeqCst);
                    if let Some(t) = throttle.lock().unwrap().as_ref() {
                        t.interrupt_waits();
                    }
                }),
            ))
        };
        *throttle.lock().unwrap() = Some(Arc::clone(&t));
        t.check();
        assert_eq!(slices.load(Ordering::SeqCst), 1);
        // Break the Arc cycle.
        throttle.lock().unwrap().take();
    }

    // ── nw-812 re-review N1: sampling between indexes ────────────────────

    /// One hour idle, then 80 s of outside CPU (embedding, reconcilers) in
    /// the final 100 s, then an index. `sample` is the daemon's periodic
    /// sampler (every SUB_WINDOW or less).
    fn hour_idle_then_late_burn(rig: &Rig, sampled: bool) {
        for step in 0..360 {
            let cpu = if step >= 350 { 8 } else { 0 };
            rig.advance(Duration::from_secs(10), Duration::from_secs(cpu));
            if sampled {
                rig.throttle.sample();
            }
        }
    }

    fn eleven_core_burst(rig: &Rig) -> Duration {
        let before = rig.total_sleep();
        for _ in 0..660 {
            rig.throttle.check();
            rig.advance(
                Duration::from_secs_f64(6.0 / 660.0),
                Duration::from_millis(100),
            );
        }
        rig.total_sleep() - before
    }

    #[test]
    fn sampled_late_outside_cpu_is_billed_where_it_happened() {
        let rig = sim_rig();
        hour_idle_then_late_burn(&rig, true);
        assert!(
            eleven_core_burst(&rig) > Duration::ZERO,
            "80 s of recent outside CPU + a 66 s burst is over budget and must wait"
        );
    }

    #[test]
    fn unsampled_gap_is_spread_evenly_until_the_first_sample() {
        // Without a sampler the gap's CPU is spread over the whole gap, so
        // only ~4 s of the late 80 s lands in the window: the burst runs free.
        // This is why the daemon samples every sub-window; spreading only
        // matters before the first sample (CPU since process start).
        let rig = sim_rig();
        hour_idle_then_late_burn(&rig, false);
        assert_eq!(eleven_core_burst(&rig), Duration::ZERO);
    }
}
