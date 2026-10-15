//! CPU budget for the parallel indexing phases.
//!
//! A daemon that saturates every core with tree-sitter parses can trip
//! runningboardd's CPU limit on macOS (observed: killed after a 90 s-CPU /
//! 180 s-window violation mid-index). `CpuThrottle` enforces that limit the
//! way it is defined: it tracks process CPU time (`getrusage(RUSAGE_SELF)`,
//! all threads summed) over a rolling 180 s window and only sleeps when the
//! window's total would exceed a budget set safely below the kill line.
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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Environment variable selecting the budget, as the average percentage of
/// ONE core the indexing process may use over the rolling 180 s window (the
/// same unit macOS reports for its own limit: 90 s / 180 s is 50%). `0` or
/// `>= 100` disables the throttle; values above [`MAX_PERCENT`] are clamped.
const ENV_VAR: &str = "NESTWEAVER_INDEX_CPU_PERCENT";

/// Default budget: 40% of one core over 180 s = 72 s of CPU per window,
/// 20% under the 90 s kill line and enough that a forced index of a large
/// repo (an 11-core burst of a few seconds) runs unthrottled.
const DEFAULT_PERCENT: u32 = 40;

/// Ceiling for configured values: 45% = 81 s per window. Anything higher
/// (including the old default of 50) would sit at or over the kill line, so
/// it is clamped here rather than honoured.
const MAX_PERCENT: u32 = 45;

/// The rolling window runningboardd measures CPU over.
const WINDOW: Duration = Duration::from_secs(180);

/// Bucket width for the rolling window.
const SUB_WINDOW: Duration = Duration::from_secs(10);

/// The most one [`CpuThrottle::check`] call will ever wait: the longest any
/// debt can take to roll out of the window. Waiting longer could only mean
/// CPU from outside the throttled work keeps refilling the window, and then
/// the worker proceeds rather than block indefinitely.
const MAX_WAIT_PER_CHECK: Duration = Duration::from_secs(190);

/// Upper bound on a single sleep slice, so a waiting worker re-measures and
/// notices cancellation promptly.
const MAX_SLEEP: Duration = Duration::from_millis(250);

/// Shared rolling-window CPU budget. Interior mutability (a `Mutex` over the
/// window) lets one `&CpuThrottle` be captured by every rayon worker.
pub struct CpuThrottle {
    /// CPU time allowed per rolling [`WINDOW`]; `None` disables the throttle.
    target: Option<Duration>,
    wall_clock: Box<dyn Fn() -> Duration + Send + Sync>,
    cpu_clock: Box<dyn Fn() -> Duration + Send + Sync>,
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
    window: Mutex<Window>,
    /// nw-812 test seam: how many times [`check`](Self::check) ran.
    #[cfg(test)]
    checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// CPU consumed per [`SUB_WINDOW`] bucket since the throttle was created.
struct Window {
    /// Process CPU time at the last measurement.
    last_cpu: Duration,
    /// `(bucket index, CPU billed to it)`, oldest first. Bucket `i` covers
    /// wall time `[i * SUB_WINDOW, (i + 1) * SUB_WINDOW)`.
    buckets: VecDeque<(u64, Duration)>,
}

impl Window {
    /// Bill CPU used since the last measurement to the current bucket and
    /// drop buckets that have rolled entirely out of the window. A bucket is
    /// kept while any part of it is inside the window, so the measured total
    /// covers up to WINDOW + SUB_WINDOW — conservative, never under-counting.
    fn update(&mut self, now_wall: Duration, now_cpu: Duration) {
        let used = now_cpu.saturating_sub(self.last_cpu);
        self.last_cpu = self.last_cpu.max(now_cpu);
        let current = bucket_index(now_wall);
        match self.buckets.back_mut() {
            Some((index, cpu)) if *index == current => *cpu += used,
            _ => self.buckets.push_back((current, used)),
        }
        while let Some(&(index, _)) = self.buckets.front() {
            if bucket_expiry(index) <= now_wall {
                self.buckets.pop_front();
            } else {
                break;
            }
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

/// Wall time at which bucket `index` has fully left the rolling window.
fn bucket_expiry(index: u64) -> Duration {
    SUB_WINDOW * u32::try_from(index + 1).unwrap_or(u32::MAX) + WINDOW
}

impl CpuThrottle {
    /// Build from `NESTWEAVER_INDEX_CPU_PERCENT` (default 40; `0` or
    /// `>= 100` produces an inert throttle whose [`check`](Self::check)
    /// never sleeps).
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var(ENV_VAR).ok().as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Self {
        let percent = value
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_PERCENT);
        let target = (percent > 0 && percent < 100)
            .then(|| WINDOW.mul_f64(f64::from(percent.min(MAX_PERCENT)) / 100.0));
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
            last_cpu: cpu_clock(),
            buckets: VecDeque::new(),
        };
        let throttle = Self {
            target,
            wall_clock,
            cpu_clock,
            sleep,
            window: Mutex::new(window),
            #[cfg(test)]
            checks: std::sync::Arc::default(),
        };
        #[cfg(test)]
        test_seam::register(&throttle.checks);
        throttle
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
        let mut slept = Duration::ZERO;
        while slept < MAX_WAIT_PER_CHECK {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return;
            }
            let now_wall = (self.wall_clock)();
            let now_cpu = (self.cpu_clock)();
            let wait = {
                // A poisoned lock only means another worker panicked
                // mid-check; the window is still usable, so recover.
                let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
                window.update(now_wall, now_cpu);
                if window.total() <= budget {
                    return;
                }
                window.wait_until_within(budget, now_wall)
            };
            let nap = wait.min(MAX_SLEEP).min(MAX_WAIT_PER_CHECK - slept);
            if nap.is_zero() {
                return;
            }
            if slept.is_zero() {
                tracing::debug!(
                    budget_secs = budget.as_secs_f64(),
                    wait_secs = wait.as_secs_f64(),
                    "index CPU budget for the rolling window spent; waiting"
                );
            }
            (self.sleep)(nap);
            slept += nap;
        }
    }
}

/// nw-812 test seam: the most recently created throttle's check counter, per
/// creating thread, so an index test can observe its own throttle.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static LAST: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
    }

    pub(super) fn register(checks: &Arc<AtomicUsize>) {
        LAST.with(|last| *last.borrow_mut() = Some(Arc::clone(checks)));
    }

    /// Checks made so far by the last throttle created on this thread.
    pub(crate) fn last_throttle_checks() -> Option<usize> {
        LAST.with(|last| last.borrow().as_ref().map(|c| c.load(Ordering::SeqCst)))
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
    fn env_values_at_or_over_the_kill_line_are_clamped() {
        // 50% would be exactly runningboardd's 90 s / 180 s limit.
        assert_eq!(CpuThrottle::from_env_value(Some("50")).target, secs(81));
        assert_eq!(CpuThrottle::from_env_value(Some("99")).target, secs(81));
    }

    #[test]
    fn env_value_is_trimmed() {
        let t = CpuThrottle::from_env_value(Some(" 45 "));
        assert_eq!(t.target, secs(81));
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
}
