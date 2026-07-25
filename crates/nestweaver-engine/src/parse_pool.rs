//! Dedicated rayon pool for the CPU-bound parse phase.
//!
//! Running the parse `par_iter` on rayon's global pool starves the daemon's
//! query serving, and its workers inherit the default scheduling priority —
//! on macOS that contributed to runningboardd killing the daemon for a CPU
//! violation mid-index. A private pool keeps parse work off the global pool
//! and, on macOS, demotes its workers to `QOS_CLASS_UTILITY` so the
//! scheduler prefers foreground work (and the power-efficient cores) for
//! everything else.

use std::sync::OnceLock;

/// Run `work` with the dedicated parse pool as the current rayon pool.
///
/// Pool construction is attempted once per process; if it fails (thread
/// spawn limits, sandboxing), `work` runs on rayon's global pool instead —
/// an index must never fail because the nice-to-have pool was unavailable.
pub fn install_parse_pool<F, R>(work: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    match parse_pool() {
        Some(pool) => pool.install(work),
        None => work(),
    }
}

fn parse_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let builder = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("nw-parse-{i}"));
        #[cfg(target_os = "macos")]
        let builder = builder.start_handler(|_| {
            // Demote each worker to utility QoS for its whole lifetime.
            // Return value is a KernReturn-style status; nothing actionable
            // to do on failure, so ignore it.
            unsafe {
                pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
            }
        });
        builder.build().ok()
    })
    .as_ref()
}

// `pthread_set_qos_class_self_np` is not exposed by the `libc` crate, and a
// `mach2` dependency for one constant is not worth it — declare it directly.
// Value from <sys/qos.h>: QOS_CLASS_UTILITY = 0x11 (note: 0x15 is
// QOS_CLASS_DEFAULT), relative priority 0 = class default.
#[cfg(target_os = "macos")]
const QOS_CLASS_UTILITY: u32 = 0x11;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_is_available_and_named() {
        let name = install_parse_pool(|| {
            rayon::current_num_threads();
            std::thread::current().name().map(str::to_owned)
        });
        if parse_pool().is_some() {
            assert_eq!(name.as_deref(), Some("nw-parse-0"));
        }
    }

    #[test]
    fn install_returns_closure_result() {
        assert_eq!(install_parse_pool(|| 42), 42);
    }
}
