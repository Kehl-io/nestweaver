//! Shared operational limits used by more than one crate.

/// Default ceiling for the daemon's graceful drain (seconds). All three
/// consumers (CLI stop-grace, daemon drain loop, client wait_for_exit)
/// key off `NESTWEAVER_DRAIN_TIMEOUT_SECS` at runtime; this is the shared
/// fallback when the env var is absent/invalid.
pub const DEFAULT_DRAIN_CEILING_SECS: u64 = 660;

/// Parse a raw `NESTWEAVER_DRAIN_TIMEOUT_SECS` value: trim surrounding
/// whitespace and fall back to [`DEFAULT_DRAIN_CEILING_SECS`] when the string
/// is not a valid number. Shared so the daemon drain loop, the client
/// wait-for-exit, and the CLI stop-grace derivation cannot drift on parse
/// semantics (only the value was single-sourced before).
pub fn parse_drain_ceiling(raw: &str) -> u64 {
    raw.trim()
        .parse::<u64>()
        .unwrap_or(DEFAULT_DRAIN_CEILING_SECS)
}

/// Read `NESTWEAVER_DRAIN_TIMEOUT_SECS` from the environment, falling back to
/// the shared default when unset or invalid.
pub fn drain_ceiling_from_env() -> u64 {
    std::env::var("NESTWEAVER_DRAIN_TIMEOUT_SECS")
        .ok()
        .as_deref()
        .map(parse_drain_ceiling)
        .unwrap_or(DEFAULT_DRAIN_CEILING_SECS)
}

/// Longest source (vault or repo) display name an interface accepts, in chars.
pub const MAX_SOURCE_NAME_CHARS: usize = 255;

/// Why a source display name was refused. A stored name is echoed by every
/// vault listing, so a name the store cannot read back (an embedded NUL trips
/// the corruption canary) broke `brain_status`, `brain_context` and even the
/// removal that would repair it.
pub fn source_name_problem(name: &str) -> Option<String> {
    if let Some(c) = name.chars().find(|c| c.is_control()) {
        return Some(format!(
            "source name must not contain control characters (found {})",
            c.escape_unicode()
        ));
    }
    let chars = name.chars().count();
    if chars > MAX_SOURCE_NAME_CHARS {
        return Some(format!(
            "source name is {chars} characters; the limit is {MAX_SOURCE_NAME_CHARS}"
        ));
    }
    None
}

/// Interface-level check for an explicitly supplied source name: everything
/// [`source_name_problem`] refuses, plus a name that is empty after trimming.
pub fn validate_explicit_source_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("source name must not be empty".to_string());
    }
    source_name_problem(name).map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_refuse_control_characters_length_and_blank() {
        assert!(validate_explicit_source_name("My Vault").is_ok());
        for bad in ["\u{0}evil", "a\rb", "a\nb", "\u{1b}[31m", "", "   "] {
            assert!(validate_explicit_source_name(bad).is_err(), "{bad:?}");
        }
        let long = "x".repeat(MAX_SOURCE_NAME_CHARS + 1);
        assert!(
            validate_explicit_source_name(&long)
                .unwrap_err()
                .contains("255")
        );
        assert!(validate_explicit_source_name(&"é".repeat(MAX_SOURCE_NAME_CHARS)).is_ok());
        // The store-level check lets an empty (defaulted) name through.
        assert!(source_name_problem("").is_none());
    }

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
        // (parse semantics themselves are covered env-free by
        // `parse_drain_ceiling_cases` below.)
        unsafe {
            match prior {
                Some(v) => std::env::set_var("NESTWEAVER_DRAIN_TIMEOUT_SECS", v),
                None => std::env::remove_var("NESTWEAVER_DRAIN_TIMEOUT_SECS"),
            }
        }
    }

    #[test]
    fn parse_drain_ceiling_cases() {
        assert_eq!(parse_drain_ceiling("120"), 120);
        assert_eq!(
            parse_drain_ceiling("  45  "),
            45,
            "surrounding whitespace trimmed"
        );
        assert_eq!(
            parse_drain_ceiling("not-a-number"),
            DEFAULT_DRAIN_CEILING_SECS
        );
        assert_eq!(parse_drain_ceiling(""), DEFAULT_DRAIN_CEILING_SECS);
    }
}
