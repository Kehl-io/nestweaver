//! Operator-facing environment variables: parse, and say so when a value is
//! ignored.
//!
//! Every tuning variable (`NESTWEAVER_*_SECS`, `NESTWEAVER_LBUG_*`,
//! `NESTWEAVER_INDEX_CPU_PERCENT`, ...) used to fall back to its default on an
//! unparsable value without a word, so `NESTWEAVER_RPC_TIMEOUT_SECS=5m` looked
//! accepted and did nothing. These helpers keep the fallback but warn, once
//! per variable per process, naming the variable and the value.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

/// Read and parse `name`. Unset, empty or all-whitespace is `None` (unset);
/// an unparsable value is `None` plus a one-time warning.
pub fn env_parse<T: FromStr>(name: &str) -> Option<T> {
    parse_env_value(name, std::env::var(name).ok().as_deref())
}

/// [`env_parse`] on a value already read, so callers that take the raw string
/// for testability still warn.
pub fn parse_env_value<T: FromStr>(name: &str, raw: Option<&str>) -> Option<T> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<T>() {
        Ok(value) => Some(value),
        Err(_) => {
            warn_invalid_env_once(name, raw, "not a valid value");
            None
        }
    }
}

/// Warn (once per variable per process) that `name=raw` is ignored.
pub fn warn_invalid_env_once(name: &str, raw: &str, why: &str) {
    if first_warning_for(name) {
        tracing::warn!("ignoring {name}={raw:?}: {why}; using the default");
    }
}

fn first_warning_for(name: &str) -> bool {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    match warned.lock() {
        Ok(mut set) => set.insert(name.to_string()),
        // A poisoned set only costs a repeated warning.
        Err(poisoned) => poisoned.into_inner().insert(name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trims_and_treats_blank_as_unset() {
        assert_eq!(parse_env_value::<u64>("NW_TEST_A", Some(" 42 ")), Some(42));
        assert_eq!(parse_env_value::<u64>("NW_TEST_A", Some("")), None);
        assert_eq!(parse_env_value::<u64>("NW_TEST_A", Some("   ")), None);
        assert_eq!(parse_env_value::<u64>("NW_TEST_A", None), None);
    }

    #[test]
    fn an_invalid_value_falls_back_and_warns_once_per_variable() {
        assert_eq!(parse_env_value::<u64>("NW_TEST_B", Some("5m")), None);
        assert!(!first_warning_for("NW_TEST_B"), "the first parse warned");
        assert_eq!(parse_env_value::<u64>("NW_TEST_B", Some("-1")), None);
        assert!(first_warning_for("NW_TEST_C"), "another variable warns too");
    }
}
