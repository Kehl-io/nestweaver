//! Shared operational limits used by more than one crate.

/// Default ceiling for the daemon's graceful drain (seconds). All three
/// consumers (CLI stop-grace, daemon drain loop, client wait_for_exit)
/// key off `NESTWEAVER_DRAIN_TIMEOUT_SECS` at runtime; this is the shared
/// fallback when the env var is absent/invalid.
pub const DEFAULT_DRAIN_CEILING_SECS: u64 = 660;

/// Parse `NESTWEAVER_DRAIN_TIMEOUT_SECS`, falling back to the shared default.
/// Trims surrounding whitespace to match the CLI's stop-grace parsing.
pub fn drain_ceiling_from_env() -> u64 {
    std::env::var("NESTWEAVER_DRAIN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_CEILING_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single test fn that exercises all cases sequentially: this is the only
    // test in the crate touching NESTWEAVER_DRAIN_TIMEOUT_SECS, so serializing
    // within one fn (and restoring prior state at the end) avoids races under
    // cargo test's parallel execution without needing a process-wide lock.
    #[test]
    fn drain_ceiling_from_env_cases() {
        let prior = std::env::var("NESTWEAVER_DRAIN_TIMEOUT_SECS").ok();

        // Default when unset.
        unsafe {
            std::env::remove_var("NESTWEAVER_DRAIN_TIMEOUT_SECS");
        }
        assert_eq!(drain_ceiling_from_env(), DEFAULT_DRAIN_CEILING_SECS);

        // Valid value is used.
        unsafe {
            std::env::set_var("NESTWEAVER_DRAIN_TIMEOUT_SECS", "120");
        }
        assert_eq!(drain_ceiling_from_env(), 120);

        // Whitespace-padded value is trimmed (parity with CLI stop-grace parsing).
        unsafe {
            std::env::set_var("NESTWEAVER_DRAIN_TIMEOUT_SECS", " 45 ");
        }
        assert_eq!(drain_ceiling_from_env(), 45);

        // Garbage falls back to the default.
        unsafe {
            std::env::set_var("NESTWEAVER_DRAIN_TIMEOUT_SECS", "not-a-number");
        }
        assert_eq!(drain_ceiling_from_env(), DEFAULT_DRAIN_CEILING_SECS);

        // Restore prior state.
        unsafe {
            match prior {
                Some(v) => std::env::set_var("NESTWEAVER_DRAIN_TIMEOUT_SECS", v),
                None => std::env::remove_var("NESTWEAVER_DRAIN_TIMEOUT_SECS"),
            }
        }
    }
}
