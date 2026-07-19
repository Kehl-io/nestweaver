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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// In-memory extension store: node UID → property map.
pub type ExtensionStore = HashMap<String, HashMap<String, serde_json::Value>>;

const INSTANCE_MIGRATION_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalUidRemap {
    source_uid: String,
    destination_uid: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InstanceExtensionMigrationJournal {
    version: u32,
    from_id: String,
    to_id: String,
    mappings: Vec<JournalUidRemap>,
}

/// Durable two-phase extension migration prepared before an instance graph
/// merge. The journal is intentionally opaque to callers: completion must use
/// [`finalize_instance_extension_migration`] so source keys are removed only
/// after their destination properties have been durably published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceExtensionMigration {
    journal: Option<InstanceExtensionMigrationJournal>,
}

impl InstanceExtensionMigration {
    pub fn is_active(&self) -> bool {
        self.journal.is_some()
    }
}

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
    write_extension_store_durable(&path, store)
}

fn load_extensions_strict(path: &Path) -> Result<Option<ExtensionStore>, anyhow::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "read extension sidecar {}: {error}",
                path.display()
            ));
        }
    };
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("parse extension sidecar {}: {error}", path.display()))
}

fn write_extension_store_durable(path: &Path, store: &ExtensionStore) -> Result<(), anyhow::Error> {
    let json = serde_json::to_vec_pretty(store)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(path, |file| file.write_all(&json))
        .map_err(|error| {
            anyhow::anyhow!(
                "durably replace extension sidecar {}: {error}",
                path.display()
            )
        })
}

fn instance_extension_migration_journal_path(db_path: &Path) -> std::path::PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(".extensions.migration.json");
    std::path::PathBuf::from(path)
}

fn load_instance_extension_migration_journal(
    path: &Path,
) -> Result<Option<InstanceExtensionMigrationJournal>, anyhow::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "read instance extension migration journal {}: {error}",
                path.display()
            ));
        }
    };
    let journal: InstanceExtensionMigrationJournal =
        serde_json::from_str(&content).map_err(|error| {
            anyhow::anyhow!(
                "parse instance extension migration journal {}: {error}",
                path.display()
            )
        })?;
    if journal.version != INSTANCE_MIGRATION_VERSION {
        anyhow::bail!(
            "unsupported instance extension migration journal version {} in {}",
            journal.version,
            path.display()
        );
    }
    validate_journal_mappings(&journal.mappings)?;
    Ok(Some(journal))
}

fn write_instance_extension_migration_journal(
    path: &Path,
    journal: &InstanceExtensionMigrationJournal,
) -> Result<(), anyhow::Error> {
    let json = serde_json::to_vec_pretty(journal)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(path, |file| file.write_all(&json))
        .map_err(|error| {
            anyhow::anyhow!(
                "durably replace instance extension migration journal {}: {error}",
                path.display()
            )
        })
}

fn validate_journal_mappings(mappings: &[JournalUidRemap]) -> Result<(), anyhow::Error> {
    if mappings.is_empty() {
        anyhow::bail!("instance extension migration journal must contain at least one UID remap");
    }
    let mut by_source = BTreeMap::new();
    let sources: BTreeSet<&str> = mappings
        .iter()
        .map(|mapping| mapping.source_uid.as_str())
        .collect();
    for mapping in mappings {
        if mapping.source_uid.is_empty() || mapping.destination_uid.is_empty() {
            anyhow::bail!("instance extension UID remaps must not contain empty UIDs");
        }
        if mapping.source_uid == mapping.destination_uid {
            anyhow::bail!(
                "instance extension UID remap source equals destination: {}",
                mapping.source_uid
            );
        }
        if sources.contains(mapping.destination_uid.as_str()) {
            anyhow::bail!(
                "instance extension UID remap chains are unsupported: {} is also a source",
                mapping.destination_uid
            );
        }
        if let Some(existing) = by_source.insert(&mapping.source_uid, &mapping.destination_uid)
            && existing != &mapping.destination_uid
        {
            anyhow::bail!(
                "instance extension UID {} maps to both {} and {}",
                mapping.source_uid,
                existing,
                mapping.destination_uid
            );
        }
    }
    Ok(())
}

fn merge_extension_properties(store: &mut ExtensionStore, mappings: &[JournalUidRemap]) -> bool {
    let source_properties: BTreeMap<String, HashMap<String, serde_json::Value>> = mappings
        .iter()
        .filter_map(|mapping| {
            store
                .get(&mapping.source_uid)
                .cloned()
                .map(|properties| (mapping.source_uid.clone(), properties))
        })
        .collect();
    let mut changed = false;
    for mapping in mappings {
        let Some(properties) = source_properties.get(&mapping.source_uid) else {
            continue;
        };
        let destination = store.entry(mapping.destination_uid.clone()).or_default();
        let mut property_names: Vec<&String> = properties.keys().collect();
        property_names.sort();
        for name in property_names {
            if !destination.contains_key(name) {
                destination.insert(name.clone(), properties[name].clone());
                changed = true;
            }
        }
    }
    changed
}

/// Durably stage authored extension properties before an instance graph merge.
///
/// Destination properties win exact key conflicts. Otherwise source
/// properties fill missing keys. Mappings are processed by sorted source UID,
/// so many-to-one collisions are deterministic. Source entries remain in the
/// sidecar until graph mutation succeeds. A durable journal preserves the
/// mapping even if the graph merge partially commits and source rows vanish.
pub fn prepare_instance_extension_migration(
    db_path: &Path,
    from_id: &str,
    to_id: &str,
    mappings: &[nestweaver_store::InstanceUidRemap],
) -> Result<InstanceExtensionMigration, anyhow::Error> {
    let extension_path = sidecar_path(db_path);
    let journal_path = instance_extension_migration_journal_path(db_path);
    let existing_journal = load_instance_extension_migration_journal(&journal_path)?;
    if existing_journal.is_none() {
        // A prior journal unlink may have reached the canonical namespace but
        // failed its final directory sync. Confirming the absent namespace on
        // every new preparation makes a retry durable without needing a
        // source graph row to rediscover the old plan.
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&journal_path).map_err(
            |error| {
                anyhow::anyhow!(
                    "confirm absent instance extension migration journal {}: {error}",
                    journal_path.display()
                )
            },
        )?;
    }
    let Some(mut store) = load_extensions_strict(&extension_path)? else {
        if existing_journal.is_some() {
            anyhow::bail!(
                "instance extension migration journal {} exists but extension sidecar {} is missing",
                journal_path.display(),
                extension_path.display()
            );
        }
        return Ok(InstanceExtensionMigration { journal: None });
    };

    if let Some(journal) = &existing_journal
        && (journal.from_id != from_id || journal.to_id != to_id)
    {
        anyhow::bail!(
            "unfinished instance extension migration {} -> {} conflicts with requested {} -> {}",
            journal.from_id,
            journal.to_id,
            from_id,
            to_id
        );
    }

    let mut combined: BTreeMap<String, String> = existing_journal
        .as_ref()
        .into_iter()
        .flat_map(|journal| journal.mappings.iter())
        .map(|mapping| (mapping.source_uid.clone(), mapping.destination_uid.clone()))
        .collect();
    for mapping in mappings {
        if !store.contains_key(&mapping.source_uid) {
            continue;
        }
        if let Some(existing) =
            combined.insert(mapping.source_uid.clone(), mapping.destination_uid.clone())
            && existing != mapping.destination_uid
        {
            anyhow::bail!(
                "instance extension UID {} maps to both {} and {}",
                mapping.source_uid,
                existing,
                mapping.destination_uid
            );
        }
    }
    if combined.is_empty() {
        return Ok(InstanceExtensionMigration { journal: None });
    }

    let mappings: Vec<JournalUidRemap> = combined
        .into_iter()
        .map(|(source_uid, destination_uid)| JournalUidRemap {
            source_uid,
            destination_uid,
        })
        .collect();
    validate_journal_mappings(&mappings)?;
    let journal = InstanceExtensionMigrationJournal {
        version: INSTANCE_MIGRATION_VERSION,
        from_id: from_id.to_string(),
        to_id: to_id.to_string(),
        mappings,
    };
    if existing_journal.as_ref() != Some(&journal) {
        write_instance_extension_migration_journal(&journal_path, &journal)?;
    }
    if merge_extension_properties(&mut store, &journal.mappings) {
        write_extension_store_durable(&extension_path, &store)?;
    } else {
        // Confirm a complete canonical replacement from an earlier attempt
        // whose final parent sync may have failed.
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&extension_path).map_err(
            |error| {
                anyhow::anyhow!(
                    "confirm staged extension sidecar {}: {error}",
                    extension_path.display()
                )
            },
        )?;
    }
    Ok(InstanceExtensionMigration {
        journal: Some(journal),
    })
}

/// Complete a staged instance extension migration after graph success.
///
/// The destination merge is re-applied before source removal, making retries
/// idempotent if a prior attempt published one sidecar replacement but failed
/// its final directory sync. The journal is durably unlinked only after the
/// replacement without source keys is durable.
pub fn finalize_instance_extension_migration(
    db_path: &Path,
    migration: &InstanceExtensionMigration,
) -> Result<(), anyhow::Error> {
    let Some(journal) = &migration.journal else {
        return Ok(());
    };
    let extension_path = sidecar_path(db_path);
    let journal_path = instance_extension_migration_journal_path(db_path);
    let mut store = load_extensions_strict(&extension_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "extension sidecar {} disappeared during instance migration",
            extension_path.display()
        )
    })?;
    let mut changed = merge_extension_properties(&mut store, &journal.mappings);
    for mapping in &journal.mappings {
        changed |= store.remove(&mapping.source_uid).is_some();
    }
    if changed {
        write_extension_store_durable(&extension_path, &store)?;
    } else {
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&extension_path).map_err(
            |error| {
                anyhow::anyhow!(
                    "confirm finalized extension sidecar {}: {error}",
                    extension_path.display()
                )
            },
        )?;
    }
    nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&journal_path).map_err(
        |error| {
            anyhow::anyhow!(
                "durably remove instance extension migration journal {}: {error}",
                journal_path.display()
            )
        },
    )?;
    Ok(())
}

/// Remove exactly one UID from the extension sidecar and durably publish the
/// replacement. Missing sidecars and missing UIDs are confirmed no-ops.
///
/// Unlike [`load_extensions`], this mutation path is strict: unreadable or
/// malformed input is returned as an error so cleanup can never overwrite
/// unrelated metadata with an empty map.
pub fn remove_extension_uid_durable(db_path: &Path, uid: &str) -> Result<bool, anyhow::Error> {
    let path = sidecar_path(db_path);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "read extension sidecar {}: {error}",
                path.display()
            ));
        }
    };
    let mut store: ExtensionStore = serde_json::from_str(&content)
        .map_err(|error| anyhow::anyhow!("parse extension sidecar {}: {error}", path.display()))?;
    if store.remove(uid).is_none() {
        // A prior atomic replacement may have reached the canonical path but
        // failed its final parent-directory sync. Re-syncing the namespace on
        // an already-absent retry confirms that replacement without rewriting
        // unrelated metadata.
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&path).map_err(
            |error| {
                anyhow::anyhow!(
                    "confirm durable extension metadata state for {uid} in {}: {error}",
                    path.display()
                )
            },
        )?;
        return Ok(false);
    }

    let json = serde_json::to_vec_pretty(&store)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| file.write_all(&json))
        .map_err(|error| {
            anyhow::anyhow!(
                "durably remove extension metadata for {uid} from {}: {error}",
                path.display()
            )
        })?;
    Ok(true)
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

    #[test]
    fn durable_uid_removal_preserves_unrelated_metadata_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let mut store = ExtensionStore::new();
        set_property(
            &mut store,
            "proj:test:remove",
            "external_refs",
            serde_json::json!(["ticket-1"]),
        );
        set_property(
            &mut store,
            "proj:test:keep",
            "tags",
            serde_json::json!(["keep"]),
        );
        save_extensions(&db_path, &store).unwrap();

        assert!(remove_extension_uid_durable(&db_path, "proj:test:remove").unwrap());
        assert!(!remove_extension_uid_durable(&db_path, "proj:test:remove").unwrap());

        let reopened = load_extensions(&db_path);
        assert!(!reopened.contains_key("proj:test:remove"));
        assert_eq!(
            get_property(&reopened, "proj:test:keep", "tags"),
            Some(&serde_json::json!(["keep"]))
        );
    }

    #[test]
    fn durable_uid_removal_propagates_corrupt_input_without_rewriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let extension_path = sidecar_path(&db_path);
        std::fs::write(&extension_path, b"{not-json").unwrap();

        let error = remove_extension_uid_durable(&db_path, "proj:test:remove").unwrap_err();

        assert!(error.to_string().contains("parse extension sidecar"));
        assert_eq!(std::fs::read(&extension_path).unwrap(), b"{not-json");
    }

    #[test]
    fn instance_migration_is_two_phase_deterministic_and_retryable() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let destination = "repo:new:aaaaaaaaaaaa";
        let source_a = "repo:old:aaaaaaaaaaaa";
        let source_z = "repo:old:zzzzzzzzzzzz";
        let source_project = "proj:old:bbbbbbbbbbbb";
        let destination_project = "proj:new:bbbbbbbbbbbb";
        let unrelated = "note:vlt:other:cccccccccccc";
        let malformed = "repo:old:not:a:canonical:uid";

        let mut store = ExtensionStore::new();
        set_property(
            &mut store,
            destination,
            "owner",
            serde_json::json!("destination-wins"),
        );
        set_property(
            &mut store,
            source_z,
            "owner",
            serde_json::json!("source-loses"),
        );
        set_property(
            &mut store,
            source_z,
            "fill",
            serde_json::json!({"from": "z", "nested": [1, {"ok": true}]}),
        );
        set_property(
            &mut store,
            source_a,
            "fill",
            serde_json::json!({"from": "a", "nested": [2, {"ok": false}]}),
        );
        set_property(
            &mut store,
            source_project,
            "project-only",
            serde_json::json!([{"deep": [1, 2, 3]}]),
        );
        set_property(&mut store, unrelated, "keep", serde_json::json!(true));
        set_property(&mut store, malformed, "keep", serde_json::json!("verbatim"));
        save_extensions(&db_path, &store).unwrap();

        let mappings = vec![
            InstanceUidRemap {
                source_uid: source_z.to_string(),
                destination_uid: destination.to_string(),
            },
            InstanceUidRemap {
                source_uid: source_project.to_string(),
                destination_uid: destination_project.to_string(),
            },
            InstanceUidRemap {
                source_uid: source_a.to_string(),
                destination_uid: destination.to_string(),
            },
        ];
        let migration =
            prepare_instance_extension_migration(&db_path, "old", "new", &mappings).unwrap();

        let staged = load_extensions(&db_path);
        assert!(staged.contains_key(source_a));
        assert!(staged.contains_key(source_z));
        assert_eq!(
            get_property(&staged, destination, "owner"),
            Some(&serde_json::json!("destination-wins"))
        );
        assert_eq!(
            get_property(&staged, destination, "fill"),
            Some(&serde_json::json!({"from": "a", "nested": [2, {"ok": false}]}))
        );
        assert_eq!(
            get_property(&staged, destination_project, "project-only"),
            Some(&serde_json::json!([{"deep": [1, 2, 3]}]))
        );
        assert_eq!(
            get_property(&staged, unrelated, "keep"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            get_property(&staged, malformed, "keep"),
            Some(&serde_json::json!("verbatim"))
        );

        // Simulate a crash after graph mutation: source graph rows are gone,
        // so the retry contributes no freshly-computed mappings.
        let retried = prepare_instance_extension_migration(&db_path, "old", "new", &[]).unwrap();
        assert_eq!(retried, migration);
        finalize_instance_extension_migration(&db_path, &retried).unwrap();

        let finalized = load_extensions(&db_path);
        for source in [source_a, source_z, source_project] {
            assert!(!finalized.contains_key(source));
        }
        assert!(finalized.contains_key(destination));
        assert!(finalized.contains_key(destination_project));
        assert!(finalized.contains_key(unrelated));
        assert!(finalized.contains_key(malformed));
        assert!(!instance_extension_migration_journal_path(&db_path).exists());
    }

    #[test]
    fn instance_migration_fails_closed_on_corrupt_sidecar() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let extension_path = sidecar_path(&db_path);
        std::fs::write(&extension_path, b"{not-json").unwrap();

        let error = prepare_instance_extension_migration(
            &db_path,
            "old",
            "new",
            &[InstanceUidRemap {
                source_uid: "repo:old:aaaaaaaaaaaa".to_string(),
                destination_uid: "repo:new:aaaaaaaaaaaa".to_string(),
            }],
        )
        .unwrap_err();

        assert!(error.to_string().contains("parse extension sidecar"));
        assert_eq!(std::fs::read(&extension_path).unwrap(), b"{not-json");
        assert!(!instance_extension_migration_journal_path(&db_path).exists());
    }

    #[test]
    fn instance_migration_missing_sidecar_is_noop() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let migration = prepare_instance_extension_migration(
            &db_path,
            "old",
            "new",
            &[InstanceUidRemap {
                source_uid: "repo:old:aaaaaaaaaaaa".to_string(),
                destination_uid: "repo:new:aaaaaaaaaaaa".to_string(),
            }],
        )
        .unwrap();

        assert!(!migration.is_active());
        finalize_instance_extension_migration(&db_path, &migration).unwrap();
        assert!(!sidecar_path(&db_path).exists());
        assert!(!instance_extension_migration_journal_path(&db_path).exists());
    }
}
