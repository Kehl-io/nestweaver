//! Schema extension sidecar store.
//!
//! `InstanceConfig` allows users to declare custom node properties via
//! `[schema_extensions]`. This module provides a lightweight JSON sidecar
//! (`<db>.extensions.json`) that stores and queries those properties without
//! requiring schema migrations in the LadybugDB graph.
//!
//! ## Format
//!
//! ```json
//! {
//!   "sym:repo:...:abc:42": { "team_owner": "platform", "deprecated": true },
//!   "sym:repo:...:def:7":  { "team_owner": "infra" }
//! }
//! ```
//!
//! Each top-level key is a node UID; the value is a map of property name →
//! `serde_json::Value`. Any JSON value is valid (string, number, boolean, etc.).

use std::collections::HashMap;
use std::path::Path;

/// In-memory extension store: node UID → property map.
pub type ExtensionStore = HashMap<String, HashMap<String, serde_json::Value>>;

/// Load the extension sidecar for a database at `db_path`.
///
/// Returns an empty map when the sidecar does not exist or cannot be parsed,
/// so callers can treat it as a non-fatal missing-data case.
pub fn load_extensions(db_path: &Path) -> ExtensionStore {
    let path = sidecar_path(db_path);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Persist the extension store as the sidecar file for `db_path`.
///
/// Uses a write-then-rename pattern so readers never observe a partial file.
pub fn save_extensions(db_path: &Path, store: &ExtensionStore) -> Result<(), anyhow::Error> {
    let path = sidecar_path(db_path);
    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(store)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Set a single property on a node. Creates the node entry if absent.
pub fn set_property(store: &mut ExtensionStore, uid: &str, key: &str, value: serde_json::Value) {
    store
        .entry(uid.to_string())
        .or_default()
        .insert(key.to_string(), value);
}

/// Get a single property for a node. Returns `None` if the node or property
/// is absent.
pub fn get_property<'a>(
    store: &'a ExtensionStore,
    uid: &str,
    key: &str,
) -> Option<&'a serde_json::Value> {
    store.get(uid)?.get(key)
}

/// Return the UIDs of all nodes whose `key` property equals `value`.
pub fn query_by_property<'a>(
    store: &'a ExtensionStore,
    key: &str,
    value: &serde_json::Value,
) -> Vec<&'a str> {
    store
        .iter()
        .filter(|(_, props)| props.get(key) == Some(value))
        .map(|(uid, _)| uid.as_str())
        .collect()
}

/// Return all properties stored for a node, or an empty map.
pub fn get_all_properties(store: &ExtensionStore, uid: &str) -> HashMap<String, serde_json::Value> {
    store.get(uid).cloned().unwrap_or_default()
}

/// Record the current UTC time as `last_indexed_at` for a vault.
///
/// Loads the extension sidecar, sets the property, and writes it back.
/// Best-effort: returns `Ok(timestamp)` on success, or an error on I/O
/// failure. The returned string is RFC 3339-ish UTC (e.g.
/// `2026-05-27T12:34:56Z`).
pub fn record_last_indexed_at(db_path: &Path, vault_uid: &str) -> Result<String, anyhow::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ts = format_epoch_secs(now.as_secs() as i64);
    let mut store = load_extensions(db_path);
    set_property(
        &mut store,
        vault_uid,
        "last_indexed_at",
        serde_json::Value::String(ts.clone()),
    );
    save_extensions(db_path, &store)?;
    Ok(ts)
}

/// Read the `last_indexed_at` timestamp for a vault from the extension
/// sidecar. Returns `None` when the sidecar is missing or the vault has
/// no recorded timestamp.
pub fn get_last_indexed_at(db_path: &Path, vault_uid: &str) -> Option<String> {
    let store = load_extensions(db_path);
    get_property(&store, vault_uid, "last_indexed_at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Render Unix epoch seconds as RFC 3339-ish UTC (`YYYY-MM-DDTHH:MM:SSZ`).
/// Mirrors `secs_to_ymd_hms` from `index_md.rs`.
fn format_epoch_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y } as i32;
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Canonical sidecar path: `<db>.extensions.json`.
fn sidecar_path(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".extensions.json");
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn round_trip_save_and_load() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path();

        let mut store = ExtensionStore::new();
        set_property(
            &mut store,
            "sym:r:x:1",
            "team_owner",
            serde_json::json!("platform"),
        );
        set_property(
            &mut store,
            "sym:r:x:1",
            "deprecated",
            serde_json::json!(false),
        );
        set_property(
            &mut store,
            "sym:r:y:2",
            "team_owner",
            serde_json::json!("infra"),
        );

        save_extensions(db_path, &store).unwrap();
        let loaded = load_extensions(db_path);

        assert_eq!(
            get_property(&loaded, "sym:r:x:1", "team_owner"),
            Some(&serde_json::json!("platform"))
        );
        assert_eq!(
            get_property(&loaded, "sym:r:x:1", "deprecated"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            get_property(&loaded, "sym:r:y:2", "team_owner"),
            Some(&serde_json::json!("infra"))
        );
    }

    #[test]
    fn query_by_property_finds_matching_uids() {
        let mut store = ExtensionStore::new();
        set_property(&mut store, "uid-a", "team", serde_json::json!("platform"));
        set_property(&mut store, "uid-b", "team", serde_json::json!("infra"));
        set_property(&mut store, "uid-c", "team", serde_json::json!("platform"));

        let mut hits = query_by_property(&store, "team", &serde_json::json!("platform"));
        hits.sort_unstable();
        assert_eq!(hits, vec!["uid-a", "uid-c"]);
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let store = load_extensions(Path::new("/tmp/does_not_exist_nestweaver_test.lbug"));
        assert!(store.is_empty());
    }

    #[test]
    fn get_property_returns_none_for_missing_uid() {
        let store = ExtensionStore::new();
        assert_eq!(get_property(&store, "no-such-uid", "key"), None);
    }

    #[test]
    fn record_and_get_last_indexed_at_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        // Write a dummy file so the sidecar path is deterministic.
        std::fs::write(&db_path, b"").unwrap();

        let vault_uid = "vlt:default:abc123";

        // Before recording, should return None.
        assert!(get_last_indexed_at(&db_path, vault_uid).is_none());

        // Record and verify it comes back.
        let ts = record_last_indexed_at(&db_path, vault_uid).unwrap();
        assert!(!ts.is_empty(), "timestamp should be non-empty");

        let got = get_last_indexed_at(&db_path, vault_uid);
        assert_eq!(
            got,
            Some(ts.clone()),
            "should retrieve the recorded timestamp"
        );

        // A different vault UID should still return None.
        assert!(get_last_indexed_at(&db_path, "vlt:default:other").is_none());

        // Recording again should update the timestamp.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let ts2 = record_last_indexed_at(&db_path, vault_uid).unwrap();
        assert!(
            ts2 >= ts,
            "second recording should produce an equal or later timestamp"
        );
        let got2 = get_last_indexed_at(&db_path, vault_uid);
        assert_eq!(got2, Some(ts2));
    }

    #[test]
    fn last_indexed_at_survives_other_property_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        std::fs::write(&db_path, b"").unwrap();

        let vault_uid = "vlt:default:xyz789";

        // Record a timestamp.
        let ts = record_last_indexed_at(&db_path, vault_uid).unwrap();

        // Write an unrelated property via the generic path.
        let mut store = load_extensions(&db_path);
        set_property(
            &mut store,
            "sym:r:x:1",
            "team_owner",
            serde_json::json!("platform"),
        );
        save_extensions(&db_path, &store).unwrap();

        // The vault timestamp should still be present.
        let got = get_last_indexed_at(&db_path, vault_uid);
        assert_eq!(
            got,
            Some(ts),
            "timestamp should survive unrelated property writes"
        );
    }
}
