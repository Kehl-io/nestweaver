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

/// Canonical sidecar path: `<db>.extensions.json`.
fn sidecar_path(db_path: &Path) -> std::path::PathBuf {
    crate::sidecar_path(db_path, ".extensions.json")
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
}
