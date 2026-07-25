//! CPU duty-cycle throttle for the parallel indexing phases.
//!
//! A daemon that saturates every core with tree-sitter parses trips
//! runningboardd's CPU limits on macOS (observed: killed after a 90s-CPU /
//! 180s-window violation mid-index). `CpuThrottle` keeps the process under a
//! target duty cycle: over a rolling wall-clock window it compares consumed
//! CPU time (`getrusage(RUSAGE_SELF)`, all threads summed) against elapsed
//! wall time, and sleeps just enough to return to the target ratio.

use std::sync::Mutex;
use std::time::Duration;

/// Environment variable selecting the target duty cycle, as a percentage of
/// one core (1–99). `0` or `>= 100` disables the throttle.
const ENV_VAR: &str = "NESTWEAVER_INDEX_CPU_PERCENT";

/// Default duty-cycle target when the variable is unset or unparsable
/// (mirrors the `env_secs` fallback convention in `git_cmd`).
const DEFAULT_PERCENT: u32 = 50;

/// Wall-clock window over which CPU usage is averaged before correcting.
const WINDOW: Duration = Duration::from_secs(5);

/// Upper bound on a single throttle sleep so cancellation and progress
/// reporting stay responsive.
const MAX_SLEEP: Duration = Duration::from_millis(250);

/// Shared duty-cycle throttle. Interior mutability (a `Mutex` over the
/// window start) lets one `&CpuThrottle` be captured by every rayon worker.
pub struct CpuThrottle {
    /// Target ratio of CPU time to wall time; `None` disables the throttle.
    target: Option<f64>,
    wall_clock: Box<dyn Fn() -> Duration + Send + Sync>,
    cpu_clock: Box<dyn Fn() -> Duration + Send + Sync>,
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
    window: Mutex<WindowStart>,
}

struct WindowStart {
    wall: Duration,
    cpu: Duration,
}

impl CpuThrottle {
    /// Build from `NESTWEAVER_INDEX_CPU_PERCENT` (default 50; `0` or
    /// `>= 100` produces an inert throttle whose [`check`](Self::check)
    /// never sleeps).
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var(ENV_VAR).ok().as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Self {
        let percent = value
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_PERCENT);
        let target = (percent > 0 && percent < 100).then(|| f64::from(percent) / 100.0);
        Self::new(target)
    }

    fn new(target: Option<f64>) -> Self {
        let start = std::time::Instant::now();
        Self::with_clocks(
            target,
            Box::new(move || start.elapsed()),
            Box::new(process_cpu_time),
            Box::new(std::thread::sleep),
        )
    }

    fn with_clocks(
        target: Option<f64>,
        wall_clock: Box<dyn Fn() -> Duration + Send + Sync>,
        cpu_clock: Box<dyn Fn() -> Duration + Send + Sync>,
        sleep: Box<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        let window = WindowStart {
            wall: wall_clock(),
            cpu: cpu_clock(),
        };
        Self {
            target,
            wall_clock,
            cpu_clock,
            sleep,
            window: Mutex::new(window),
        }
    }

    /// Called from parallel workers before a unit of CPU-bound work. Cheap:
    /// no-ops until the rolling window has elapsed, and entirely inert when
    /// the throttle is disabled.
    pub fn check(&self) {
        let Some(target) = self.target else { return };
        let now_wall = (self.wall_clock)();
        let now_cpu = (self.cpu_clock)();
        // A poisoned lock only means another worker panicked mid-check; the
        // window state is still usable, so recover rather than fail.
        let mut window = self.window.lock().unwrap_or_else(|p| p.into_inner());
        let wall_delta = now_wall.saturating_sub(window.wall);
        if wall_delta < WINDOW {
            return;
        }
        let cpu_delta = now_cpu.saturating_sub(window.cpu);

        // Wall time that *should* have elapsed for cpu_delta at the target
        // ratio; the shortfall is made up by sleeping (capped per call).
        let needed_wall = cpu_delta.as_secs_f64() / target;
        let extra = needed_wall - wall_delta.as_secs_f64();
        if extra <= 0.0 {
            // Under budget: start a fresh window so the next burst is judged
            // on its own WINDOW slice rather than ancient history.
            window.wall = now_wall;
            window.cpu = now_cpu;
            return;
        }
        // Over budget: do NOT reset the window. With thousands of files per
        // window and a 250ms per-call cap, resetting here would let every
        // worker burn at 100% for the next WINDOW before anyone sleeps
        // again — the correction could never reach the target duty cycle.
        // Keeping the window open means every worker observing the surplus
        // sleeps on each call until the ratio recovers.
        drop(window);
        (self.sleep)(Duration::from_secs_f64(extra).min(MAX_SLEEP));
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

    fn rig(target: Option<f64>) -> Rig {
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

    #[test]
    fn env_default_is_fifty_percent() {
        let t = CpuThrottle::from_env_value(None);
        assert_eq!(t.target, Some(0.5));
    }

    #[test]
    fn env_unparsable_falls_back_to_default() {
        let t = CpuThrottle::from_env_value(Some("garbage"));
        assert_eq!(t.target, Some(0.5));
    }

    #[test]
    fn env_zero_and_hundred_plus_disable() {
        assert_eq!(CpuThrottle::from_env_value(Some("0")).target, None);
        assert_eq!(CpuThrottle::from_env_value(Some("100")).target, None);
        assert_eq!(CpuThrottle::from_env_value(Some("800")).target, None);
    }

    #[test]
    fn env_partial_percent_is_honored() {
        let t = CpuThrottle::from_env_value(Some("25"));
        assert_eq!(t.target, Some(0.25));
    }

    #[test]
    fn env_value_is_trimmed() {
        let t = CpuThrottle::from_env_value(Some(" 45 "));
        assert_eq!(t.target, Some(0.45));
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
    fn window_must_elapse_before_evaluating() {
        let rig = rig(Some(0.5));
        // Under the window length, even a huge CPU surplus is not judged yet.
        rig.advance(Duration::from_secs(4), Duration::from_secs(30));
        rig.throttle.check();
        assert!(rig.sleeps().is_empty());
    }

    #[test]
    fn over_budget_sleeps_capped_to_max() {
        let rig = rig(Some(0.5));
        // 4s CPU over 5s wall with a 0.5 target needs 8s wall → 3s short,
        // but a single call may only sleep MAX_SLEEP.
        rig.advance(Duration::from_secs(5), Duration::from_secs(4));
        rig.throttle.check();
        assert_eq!(rig.sleeps(), vec![MAX_SLEEP]);
    }

    #[test]
    fn under_budget_does_not_sleep() {
        let rig = rig(Some(0.5));
        // 1s CPU over 5s wall is a 0.2 ratio — comfortably under target.
        rig.advance(Duration::from_secs(5), Duration::from_secs(1));
        rig.throttle.check();
        assert!(rig.sleeps().is_empty());
    }

    #[test]
    fn window_resets_after_each_evaluation() {
        let rig = rig(Some(0.5));
        rig.advance(Duration::from_secs(5), Duration::from_secs(1));
        rig.throttle.check();
        assert!(rig.sleeps().is_empty());
        // A second evaluation happens only after another full window.
        rig.advance(Duration::from_secs(1), Duration::from_secs(10));
        rig.throttle.check();
        assert!(rig.sleeps().is_empty());
    }

    #[test]
    fn over_budget_window_stays_open_until_ratio_recovers() {
        let rig = rig(Some(0.5));
        // 4s CPU over 5s wall at target 0.5 → over budget; sleeps.
        rig.advance(Duration::from_secs(5), Duration::from_secs(4));
        rig.throttle.check();
        // The window must NOT reset on over-budget: the very next call (even
        // moments later) still sees the surplus and sleeps again. This is
        // what lets the correction actually reach the target duty cycle.
        rig.advance(Duration::from_millis(100), Duration::from_millis(100));
        rig.throttle.check();
        assert_eq!(rig.sleeps(), vec![MAX_SLEEP, MAX_SLEEP]);
        // Idle wall time (workers sleeping) pays down the surplus: 9.1s wall
        // for 4.1s CPU is a ~0.45 ratio → under budget, window resets.
        rig.advance(Duration::from_secs(4), Duration::ZERO);
        rig.throttle.check();
        assert_eq!(rig.sleeps().len(), 2);
        // Window has now reset; a fresh sub-window burst is not yet judged.
        rig.advance(Duration::from_secs(1), Duration::from_secs(10));
        rig.throttle.check();
        assert_eq!(rig.sleeps().len(), 2);
    }
}
