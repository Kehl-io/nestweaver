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
use sha2::{Digest, Sha256};

/// In-memory extension store: node UID → property map.
pub type ExtensionStore = HashMap<String, HashMap<String, serde_json::Value>>;

const INSTANCE_MIGRATION_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InstanceMigrationPhase {
    Prepared,
    GraphApplied,
}

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
    phase: InstanceMigrationPhase,
    operation_id: String,
    plan_fingerprint: String,
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

    pub fn from_id(&self) -> Option<&str> {
        self.journal
            .as_ref()
            .map(|journal| journal.from_id.as_str())
    }

    pub fn to_id(&self) -> Option<&str> {
        self.journal.as_ref().map(|journal| journal.to_id.as_str())
    }

    pub fn graph_applied(&self) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|journal| journal.phase == InstanceMigrationPhase::GraphApplied)
    }

    pub fn uid_remaps(&self) -> Vec<nestweaver_store::InstanceUidRemap> {
        self.journal
            .as_ref()
            .map(|journal| {
                journal
                    .mappings
                    .iter()
                    .map(|mapping| nestweaver_store::InstanceUidRemap {
                        source_uid: mapping.source_uid.clone(),
                        destination_uid: mapping.destination_uid.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
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
    validate_instance_extension_migration_journal(&journal)
        .map_err(|error| anyhow::anyhow!("invalid journal {}: {error:#}", path.display()))?;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedUid<'a> {
    Repo {
        instance: &'a str,
        repo_hash: &'a str,
    },
    File {
        instance: &'a str,
        repo_hash: &'a str,
        path_hash: &'a str,
    },
    Symbol {
        instance: &'a str,
        repo_hash: &'a str,
        file_hash: &'a str,
        name_hash: &'a str,
        line: u32,
    },
    Project {
        instance: &'a str,
        name_hash: &'a str,
    },
}

impl ParsedUid<'_> {
    fn instance(&self) -> &str {
        match self {
            Self::Repo { instance, .. }
            | Self::File { instance, .. }
            | Self::Symbol { instance, .. }
            | Self::Project { instance, .. } => instance,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Repo { .. } => "Repo",
            Self::File { .. } => "File",
            Self::Symbol { .. } => "Symbol",
            Self::Project { .. } => "Project",
        }
    }
}

fn is_uid_hash(value: &str) -> bool {
    value.len() == 12
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_instance_migration_uid(uid: &str) -> Result<ParsedUid<'_>, anyhow::Error> {
    let parts: Vec<&str> = uid.split(':').collect();
    let invalid = || anyhow::anyhow!("non-canonical instance extension UID {uid:?}");
    match parts.as_slice() {
        ["repo", instance, repo_hash] if !instance.is_empty() && is_uid_hash(repo_hash) => {
            Ok(ParsedUid::Repo {
                instance,
                repo_hash,
            })
        }
        ["file", "repo", instance, repo_hash, path_hash]
            if !instance.is_empty() && is_uid_hash(repo_hash) && is_uid_hash(path_hash) =>
        {
            Ok(ParsedUid::File {
                instance,
                repo_hash,
                path_hash,
            })
        }
        [
            "sym",
            "repo",
            instance,
            repo_hash,
            file_hash,
            name_hash,
            line,
        ] if !instance.is_empty()
            && is_uid_hash(repo_hash)
            && is_uid_hash(file_hash)
            && is_uid_hash(name_hash) =>
        {
            let parsed_line: u32 = line.parse().map_err(|_| invalid())?;
            if parsed_line.to_string() != *line {
                return Err(invalid());
            }
            Ok(ParsedUid::Symbol {
                instance,
                repo_hash,
                file_hash,
                name_hash,
                line: parsed_line,
            })
        }
        ["proj", instance, name_hash] if !instance.is_empty() && is_uid_hash(name_hash) => {
            Ok(ParsedUid::Project {
                instance,
                name_hash,
            })
        }
        _ => Err(invalid()),
    }
}

fn validate_deterministic_destination(
    source: ParsedUid<'_>,
    destination: ParsedUid<'_>,
) -> Result<(), anyhow::Error> {
    let matches = match (source, destination) {
        (
            ParsedUid::Repo {
                repo_hash: source, ..
            },
            ParsedUid::Repo {
                repo_hash: destination,
                ..
            },
        ) => source == destination,
        (
            ParsedUid::File {
                repo_hash: source_repo,
                path_hash: source_path,
                ..
            },
            ParsedUid::File {
                repo_hash: destination_repo,
                path_hash: destination_path,
                ..
            },
        ) => source_repo == destination_repo && source_path == destination_path,
        (
            ParsedUid::Symbol {
                repo_hash: source_repo,
                file_hash: source_file,
                name_hash: source_name,
                line: source_line,
                ..
            },
            ParsedUid::Symbol {
                repo_hash: destination_repo,
                file_hash: destination_file,
                name_hash: destination_name,
                line: destination_line,
                ..
            },
        ) => {
            source_repo == destination_repo
                && source_file == destination_file
                && source_name == destination_name
                && source_line == destination_line
        }
        (ParsedUid::Project { .. }, ParsedUid::Project { .. }) => true,
        _ => false,
    };
    if !matches {
        anyhow::bail!("instance extension UID remap has a non-deterministic destination");
    }
    Ok(())
}

fn validate_journal_mappings(
    from_id: &str,
    to_id: &str,
    mappings: &[JournalUidRemap],
) -> Result<(), anyhow::Error> {
    if from_id.is_empty() || to_id.is_empty() || from_id == to_id {
        anyhow::bail!("instance extension migration IDs must be non-empty and distinct");
    }
    if mappings.is_empty() {
        anyhow::bail!("instance extension migration journal must contain at least one UID remap");
    }
    let mut previous_source: Option<&str> = None;
    let sources: BTreeSet<&str> = mappings
        .iter()
        .map(|mapping| mapping.source_uid.as_str())
        .collect();
    for mapping in mappings {
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
        if previous_source.is_some_and(|previous| previous >= mapping.source_uid.as_str()) {
            anyhow::bail!("instance extension UID remaps must be strictly source-UID sorted");
        }
        previous_source = Some(&mapping.source_uid);

        let source = parse_instance_migration_uid(&mapping.source_uid)?;
        let destination = parse_instance_migration_uid(&mapping.destination_uid)?;
        if source.kind() != destination.kind() {
            anyhow::bail!(
                "instance extension UID remap changes kind from {} to {}",
                source.kind(),
                destination.kind()
            );
        }
        if destination.instance() != to_id {
            anyhow::bail!(
                "instance extension UID remap destination belongs to {:?}, expected {:?}",
                destination.instance(),
                to_id
            );
        }
        let target_project_loser =
            matches!(source, ParsedUid::Project { .. }) && source.instance() == to_id;
        if source.instance() != from_id && !target_project_loser {
            anyhow::bail!(
                "instance extension UID remap source belongs to {:?}, expected {:?}",
                source.instance(),
                from_id
            );
        }
        // A target-instance source is explicitly a legacy duplicate Project
        // loser selected by the deterministic graph collision plan.
        validate_deterministic_destination(source, destination)?;
    }
    Ok(())
}

fn plan_fingerprint(
    from_id: &str,
    to_id: &str,
    mappings: &[JournalUidRemap],
) -> Result<String, anyhow::Error> {
    let payload = serde_json::to_vec(&(INSTANCE_MIGRATION_VERSION, from_id, to_id, mappings))?;
    let digest = Sha256::digest(payload);
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        fingerprint.push(HEX[(byte >> 4) as usize] as char);
        fingerprint.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(fingerprint)
}

fn operation_id(from_id: &str, to_id: &str, fingerprint: &str) -> String {
    format!("instance-merge:{from_id}:{to_id}:{fingerprint}")
}

fn validate_instance_extension_migration_journal(
    journal: &InstanceExtensionMigrationJournal,
) -> Result<(), anyhow::Error> {
    if journal.version != INSTANCE_MIGRATION_VERSION {
        anyhow::bail!(
            "unsupported instance extension migration journal version {}",
            journal.version
        );
    }
    validate_journal_mappings(&journal.from_id, &journal.to_id, &journal.mappings)?;
    let fingerprint = plan_fingerprint(&journal.from_id, &journal.to_id, &journal.mappings)?;
    if journal.plan_fingerprint != fingerprint {
        anyhow::bail!("instance extension migration plan fingerprint mismatch");
    }
    if journal.operation_id != operation_id(&journal.from_id, &journal.to_id, &fingerprint) {
        anyhow::bail!("instance extension migration operation identity mismatch");
    }
    Ok(())
}

fn canonical_journal_mappings(
    from_id: &str,
    to_id: &str,
    mappings: &[nestweaver_store::InstanceUidRemap],
) -> Result<Vec<JournalUidRemap>, anyhow::Error> {
    let mut mappings: Vec<_> = mappings
        .iter()
        .map(|mapping| JournalUidRemap {
            source_uid: mapping.source_uid.clone(),
            destination_uid: mapping.destination_uid.clone(),
        })
        .collect();
    mappings.sort();
    validate_journal_mappings(from_id, to_id, &mappings)?;
    Ok(mappings)
}

fn merge_extension_properties(
    store: &mut ExtensionStore,
    to_id: &str,
    mappings: &[JournalUidRemap],
) -> bool {
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
    let mut ordered_mappings: Vec<&JournalUidRemap> = mappings.iter().collect();
    ordered_mappings.sort_by(|left, right| {
        let left_is_target_project = parse_instance_migration_uid(&left.source_uid)
            .is_ok_and(|uid| matches!(uid, ParsedUid::Project { .. }) && uid.instance() == to_id);
        let right_is_target_project = parse_instance_migration_uid(&right.source_uid)
            .is_ok_and(|uid| matches!(uid, ParsedUid::Project { .. }) && uid.instance() == to_id);
        (!left_is_target_project)
            .cmp(&(!right_is_target_project))
            .then_with(|| left.source_uid.cmp(&right.source_uid))
    });
    for mapping in ordered_mappings {
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

fn journal_for_plan(
    from_id: &str,
    to_id: &str,
    mappings: Vec<JournalUidRemap>,
    phase: InstanceMigrationPhase,
) -> Result<InstanceExtensionMigrationJournal, anyhow::Error> {
    let plan_fingerprint = plan_fingerprint(from_id, to_id, &mappings)?;
    Ok(InstanceExtensionMigrationJournal {
        version: INSTANCE_MIGRATION_VERSION,
        phase,
        operation_id: operation_id(from_id, to_id, &plan_fingerprint),
        plan_fingerprint,
        from_id: from_id.to_string(),
        to_id: to_id.to_string(),
        mappings,
    })
}

/// Load and strictly validate an unfinished instance extension migration.
/// A missing journal is represented by an inactive migration.
pub fn pending_instance_extension_migration(
    db_path: &Path,
) -> Result<InstanceExtensionMigration, anyhow::Error> {
    let journal_path = instance_extension_migration_journal_path(db_path);
    let journal = load_instance_extension_migration_journal(&journal_path)?;
    if journal.is_some() && load_extensions_strict(&sidecar_path(db_path))?.is_none() {
        anyhow::bail!(
            "instance extension migration journal {} exists but extension sidecar is missing",
            journal_path.display()
        );
    }
    Ok(InstanceExtensionMigration { journal })
}

/// Durably record the exact extension migration plan before graph mutation.
///
/// Preparation does not publish destination keys, so queries cannot observe
/// duplicate old/new metadata before the graph commits. If a journal already
/// exists, the current graph plan must match it exactly; same-pair journals are
/// never unioned with newly supplied mappings.
pub fn prepare_instance_extension_migration(
    db_path: &Path,
    from_id: &str,
    to_id: &str,
    mappings: &[nestweaver_store::InstanceUidRemap],
) -> Result<InstanceExtensionMigration, anyhow::Error> {
    prepare_instance_extension_migration_with_write(
        db_path,
        from_id,
        to_id,
        mappings,
        write_instance_extension_migration_journal,
    )
}

fn prepare_instance_extension_migration_with_write<F>(
    db_path: &Path,
    from_id: &str,
    to_id: &str,
    mappings: &[nestweaver_store::InstanceUidRemap],
    write_journal: F,
) -> Result<InstanceExtensionMigration, anyhow::Error>
where
    F: FnOnce(&Path, &InstanceExtensionMigrationJournal) -> Result<(), anyhow::Error>,
{
    let extension_path = sidecar_path(db_path);
    let journal_path = instance_extension_migration_journal_path(db_path);
    let store = load_extensions_strict(&extension_path)?;
    let existing_journal = load_instance_extension_migration_journal(&journal_path)?;
    let Some(store) = store else {
        if existing_journal.is_some() {
            anyhow::bail!(
                "instance extension migration journal {} exists but extension sidecar {} is missing",
                journal_path.display(),
                extension_path.display()
            );
        }
        return Ok(InstanceExtensionMigration { journal: None });
    };

    if let Some(journal) = existing_journal {
        if journal.from_id != from_id || journal.to_id != to_id {
            anyhow::bail!(
                "unfinished instance extension migration {} -> {} conflicts with requested {} -> {}",
                journal.from_id,
                journal.to_id,
                from_id,
                to_id
            );
        }
        let current_mappings =
            canonical_journal_mappings(from_id, to_id, mappings).map_err(|error| {
                anyhow::anyhow!("exact current graph plan does not match journal: {error}")
            })?;
        if journal.phase != InstanceMigrationPhase::Prepared
            || journal.mappings != current_mappings
            || journal.plan_fingerprint != plan_fingerprint(from_id, to_id, &current_mappings)?
        {
            anyhow::bail!(
                "unfinished instance extension migration does not match the exact current graph plan"
            );
        }
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&journal_path).map_err(
            |error| {
                anyhow::anyhow!(
                    "confirm prepared journal {}: {error}",
                    journal_path.display()
                )
            },
        )?;
        return Ok(InstanceExtensionMigration {
            journal: Some(journal),
        });
    }

    if mappings.is_empty()
        || !mappings
            .iter()
            .any(|mapping| store.contains_key(&mapping.source_uid))
    {
        return Ok(InstanceExtensionMigration { journal: None });
    }
    let mappings = canonical_journal_mappings(from_id, to_id, mappings)?;
    let journal = journal_for_plan(from_id, to_id, mappings, InstanceMigrationPhase::Prepared)?;
    write_journal(&journal_path, &journal)?;
    Ok(InstanceExtensionMigration {
        journal: Some(journal),
    })
}

/// Durably record that the graph mutation for a prepared plan succeeded.
pub fn mark_instance_extension_migration_graph_applied(
    db_path: &Path,
    migration: &InstanceExtensionMigration,
) -> Result<InstanceExtensionMigration, anyhow::Error> {
    let Some(expected) = &migration.journal else {
        return Ok(migration.clone());
    };
    let journal_path = instance_extension_migration_journal_path(db_path);
    let mut current =
        load_instance_extension_migration_journal(&journal_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "instance extension migration journal {} disappeared before graph commit",
                journal_path.display()
            )
        })?;
    if &current != expected {
        anyhow::bail!("instance extension migration journal changed before graph commit");
    }
    if current.phase != InstanceMigrationPhase::Prepared {
        anyhow::bail!("instance extension migration journal was not in prepared phase");
    }
    current.phase = InstanceMigrationPhase::GraphApplied;
    write_instance_extension_migration_journal(&journal_path, &current)?;
    Ok(InstanceExtensionMigration {
        journal: Some(current),
    })
}

/// Atomically publish migrated extension properties after graph success.
///
/// Existing winner properties win exact key conflicts. Legacy target Project
/// losers fill missing keys in lexical UID order, then source-instance nodes
/// fill remaining keys in lexical UID order. The journal is durably removed
/// only after the source-free sidecar replacement is durable.
pub fn finalize_instance_extension_migration(
    db_path: &Path,
    migration: &InstanceExtensionMigration,
) -> Result<(), anyhow::Error> {
    finalize_instance_extension_migration_with_remove(db_path, migration, |path| {
        nestweaver_store::durable_sidecar::remove_file_durable_if_exists(path)
    })
}

fn finalize_instance_extension_migration_with_remove<F>(
    db_path: &Path,
    migration: &InstanceExtensionMigration,
    remove_journal: F,
) -> Result<(), anyhow::Error>
where
    F: FnOnce(&Path) -> Result<bool, std::io::Error>,
{
    let Some(journal) = &migration.journal else {
        return Ok(());
    };
    let extension_path = sidecar_path(db_path);
    let journal_path = instance_extension_migration_journal_path(db_path);
    let current = load_instance_extension_migration_journal(&journal_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "instance extension migration journal {} disappeared before completion",
            journal_path.display()
        )
    })?;
    if &current != journal {
        anyhow::bail!("instance extension migration journal changed before completion");
    }
    if journal.phase != InstanceMigrationPhase::GraphApplied {
        anyhow::bail!("instance extension migration graph is not durably marked applied");
    }
    let mut store = load_extensions_strict(&extension_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "extension sidecar {} disappeared during instance migration",
            extension_path.display()
        )
    })?;
    let mut changed = merge_extension_properties(&mut store, &journal.to_id, &journal.mappings);
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
    if let Err(error) = remove_journal(&journal_path) {
        // A durable unlink can fail after the namespace removal but before its
        // parent directory sync. Restore the validated journal so startup can
        // retry instead of silently losing the recovery plan.
        if !journal_path.exists()
            && let Err(restore_error) =
                write_instance_extension_migration_journal(&journal_path, journal)
        {
            anyhow::bail!(
                "durably remove instance extension migration journal {}: {error}; restore retry journal failed: {restore_error:#}",
                journal_path.display()
            );
        }
        anyhow::bail!(
            "durably remove instance extension migration journal {}: {error}",
            journal_path.display()
        );
    }
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
        let destination = "proj:new:000000000001";
        let target_loser = "proj:new:111111111111";
        let source_a = "proj:old:aaaaaaaaaaaa";
        let source_z = "proj:old:ffffffffffff";
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
            "source-fill",
            serde_json::json!({"from": "z", "nested": [1, {"ok": true}]}),
        );
        set_property(
            &mut store,
            source_a,
            "source-fill",
            serde_json::json!({"from": "a", "nested": [2, {"ok": false}]}),
        );
        set_property(
            &mut store,
            target_loser,
            "fill",
            serde_json::json!({"from": "target-loser"}),
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
                source_uid: target_loser.to_string(),
                destination_uid: destination.to_string(),
            },
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
        assert_eq!(get_property(&staged, destination, "fill"), None);
        assert_eq!(get_property(&staged, destination, "source-fill"), None);
        assert!(!staged.contains_key(destination_project));
        assert_eq!(
            get_property(&staged, unrelated, "keep"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            get_property(&staged, malformed, "keep"),
            Some(&serde_json::json!("verbatim"))
        );

        let graph_applied =
            mark_instance_extension_migration_graph_applied(&db_path, &migration).unwrap();
        // Simulate a crash after graph mutation: recovery reloads the exact
        // validated graph-applied journal instead of accepting an empty plan.
        let retried = pending_instance_extension_migration(&db_path).unwrap();
        assert_eq!(retried, graph_applied);
        finalize_instance_extension_migration(&db_path, &retried).unwrap();

        let finalized = load_extensions(&db_path);
        for source in [target_loser, source_a, source_z, source_project] {
            assert!(!finalized.contains_key(source));
        }
        assert!(finalized.contains_key(destination));
        assert!(finalized.contains_key(destination_project));
        assert_eq!(
            get_property(&finalized, destination, "fill"),
            Some(&serde_json::json!({"from": "target-loser"}))
        );
        assert_eq!(
            get_property(&finalized, destination, "source-fill"),
            Some(&serde_json::json!({"from": "a", "nested": [2, {"ok": false}]}))
        );
        assert_eq!(
            get_property(&finalized, destination_project, "project-only"),
            Some(&serde_json::json!([{"deep": [1, 2, 3]}]))
        );
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

    #[test]
    fn instance_migration_prepare_binds_exact_plan_without_prepublishing() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let source = "repo:old:aaaaaaaaaaaa";
        let destination = "repo:new:aaaaaaaaaaaa";
        let mut store = ExtensionStore::new();
        set_property(&mut store, source, "owner", serde_json::json!("source"));
        save_extensions(&db_path, &store).unwrap();

        let migration = prepare_instance_extension_migration(
            &db_path,
            "old",
            "new",
            &[InstanceUidRemap {
                source_uid: source.to_string(),
                destination_uid: destination.to_string(),
            }],
        )
        .unwrap();

        assert!(migration.is_active());
        let prepared = load_extensions(&db_path);
        assert!(prepared.contains_key(source));
        assert!(!prepared.contains_key(destination));
        let journal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(instance_extension_migration_journal_path(&db_path)).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["version"], INSTANCE_MIGRATION_VERSION);
        assert_eq!(journal["phase"], "prepared");
        assert_eq!(
            journal["operation_id"].as_str(),
            journal["plan_fingerprint"]
                .as_str()
                .map(|fingerprint| format!("instance-merge:old:new:{fingerprint}"))
                .as_deref()
        );
    }

    #[test]
    fn existing_instance_migration_journal_requires_the_exact_current_plan() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let first = InstanceUidRemap {
            source_uid: "repo:old:aaaaaaaaaaaa".to_string(),
            destination_uid: "repo:new:aaaaaaaaaaaa".to_string(),
        };
        let extra = InstanceUidRemap {
            source_uid: "proj:old:bbbbbbbbbbbb".to_string(),
            destination_uid: "proj:new:bbbbbbbbbbbb".to_string(),
        };
        let mut store = ExtensionStore::new();
        set_property(
            &mut store,
            &first.source_uid,
            "first",
            serde_json::json!(true),
        );
        set_property(
            &mut store,
            &extra.source_uid,
            "extra",
            serde_json::json!(true),
        );
        save_extensions(&db_path, &store).unwrap();
        prepare_instance_extension_migration(&db_path, "old", "new", std::slice::from_ref(&first))
            .unwrap();

        let extra_error =
            prepare_instance_extension_migration(&db_path, "old", "new", &[first.clone(), extra])
                .unwrap_err();
        assert!(extra_error.to_string().contains("exact current graph plan"));

        let missing_error =
            prepare_instance_extension_migration(&db_path, "old", "new", &[]).unwrap_err();
        assert!(
            missing_error
                .to_string()
                .contains("exact current graph plan")
        );
    }

    #[test]
    fn instance_migration_journal_rejects_schema_identity_and_semantic_tampering() {
        use nestweaver_store::InstanceUidRemap;

        fn wrong_version(journal: &mut serde_json::Value) {
            journal["version"] = serde_json::json!(99);
        }
        fn wrong_phase(journal: &mut serde_json::Value) {
            journal["phase"] = serde_json::json!("invented");
        }
        fn wrong_fingerprint(journal: &mut serde_json::Value) {
            journal["plan_fingerprint"] = serde_json::json!("00");
        }
        fn wrong_operation(journal: &mut serde_json::Value) {
            journal["operation_id"] = serde_json::json!("instance-merge:other");
        }
        fn wrong_kind(journal: &mut serde_json::Value) {
            journal["mappings"][0]["destination_uid"] = serde_json::json!("proj:new:aaaaaaaaaaaa");
        }
        fn wrong_source_instance(journal: &mut serde_json::Value) {
            journal["mappings"][0]["source_uid"] = serde_json::json!("repo:other:aaaaaaaaaaaa");
        }
        fn wrong_destination_instance(journal: &mut serde_json::Value) {
            journal["mappings"][0]["destination_uid"] =
                serde_json::json!("repo:other:aaaaaaaaaaaa");
        }
        fn wrong_destination_hash(journal: &mut serde_json::Value) {
            journal["mappings"][0]["destination_uid"] = serde_json::json!("repo:new:bbbbbbbbbbbb");
        }
        fn extra_mapping(journal: &mut serde_json::Value) {
            journal["mappings"].as_array_mut().unwrap().insert(
                0,
                serde_json::json!({
                    "source_uid": "proj:old:bbbbbbbbbbbb",
                    "destination_uid": "proj:new:bbbbbbbbbbbb"
                }),
            );
        }
        fn missing_mapping(journal: &mut serde_json::Value) {
            journal["mappings"].as_array_mut().unwrap().clear();
        }
        fn duplicate_mapping(journal: &mut serde_json::Value) {
            let duplicate = journal["mappings"][0].clone();
            journal["mappings"].as_array_mut().unwrap().push(duplicate);
        }
        fn chained_project_mappings(journal: &mut serde_json::Value) {
            journal["mappings"] = serde_json::json!([
                {
                    "source_uid": "proj:new:bbbbbbbbbbbb",
                    "destination_uid": "proj:new:cccccccccccc"
                },
                {
                    "source_uid": "proj:old:aaaaaaaaaaaa",
                    "destination_uid": "proj:new:bbbbbbbbbbbb"
                }
            ]);
        }
        fn mismatched_pair(journal: &mut serde_json::Value) {
            journal["from_id"] = serde_json::json!("other");
        }

        type JournalTamper = fn(&mut serde_json::Value);
        type TamperCase = (&'static str, JournalTamper);
        let cases: &[TamperCase] = &[
            ("version", wrong_version),
            ("phase", wrong_phase),
            ("fingerprint", wrong_fingerprint),
            ("operation", wrong_operation),
            ("kind", wrong_kind),
            ("source-instance", wrong_source_instance),
            ("destination-instance", wrong_destination_instance),
            ("destination-hash", wrong_destination_hash),
            ("extra", extra_mapping),
            ("missing", missing_mapping),
            ("duplicate", duplicate_mapping),
            ("chain", chained_project_mappings),
            ("pair", mismatched_pair),
        ];

        for (case, tamper) in cases {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("test.lbug");
            let source = "repo:old:aaaaaaaaaaaa";
            let mut store = ExtensionStore::new();
            set_property(&mut store, source, "owner", serde_json::json!("source"));
            save_extensions(&db_path, &store).unwrap();
            prepare_instance_extension_migration(
                &db_path,
                "old",
                "new",
                &[InstanceUidRemap {
                    source_uid: source.to_string(),
                    destination_uid: "repo:new:aaaaaaaaaaaa".to_string(),
                }],
            )
            .unwrap();
            let journal_path = instance_extension_migration_journal_path(&db_path);
            let mut journal: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
            tamper(&mut journal);
            std::fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();

            let error = pending_instance_extension_migration(&db_path)
                .unwrap_err()
                .to_string();
            assert!(!error.is_empty(), "tamper case {case} was accepted");
        }
    }

    #[test]
    fn real_journal_atomic_write_persist_failure_is_retryable_without_prepublish() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let source = "repo:old:aaaaaaaaaaaa";
        let destination = "repo:new:aaaaaaaaaaaa";
        let mapping = InstanceUidRemap {
            source_uid: source.to_string(),
            destination_uid: destination.to_string(),
        };
        let mut store = ExtensionStore::new();
        set_property(&mut store, source, "owner", serde_json::json!("source"));
        save_extensions(&db_path, &store).unwrap();

        let error = prepare_instance_extension_migration_with_write(
            &db_path,
            "old",
            "new",
            std::slice::from_ref(&mapping),
            |path, journal| {
                write_instance_extension_migration_journal(path, journal)?;
                anyhow::bail!("injected failure after real atomic journal persist")
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("after real atomic journal persist")
        );
        assert!(instance_extension_migration_journal_path(&db_path).exists());
        let unchanged = load_extensions(&db_path);
        assert!(unchanged.contains_key(source));
        assert!(!unchanged.contains_key(destination));

        let retried = prepare_instance_extension_migration(
            &db_path,
            "old",
            "new",
            std::slice::from_ref(&mapping),
        )
        .unwrap();
        assert!(retried.is_active());
    }

    #[test]
    fn real_journal_unlink_sync_failure_restores_retry_plan() {
        use nestweaver_store::InstanceUidRemap;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let source = "repo:old:aaaaaaaaaaaa";
        let destination = "repo:new:aaaaaaaaaaaa";
        let mut store = ExtensionStore::new();
        set_property(
            &mut store,
            source,
            "nested",
            serde_json::json!({"retry": [true, {"depth": 4}]}),
        );
        save_extensions(&db_path, &store).unwrap();
        let prepared = prepare_instance_extension_migration(
            &db_path,
            "old",
            "new",
            &[InstanceUidRemap {
                source_uid: source.to_string(),
                destination_uid: destination.to_string(),
            }],
        )
        .unwrap();
        let graph_applied =
            mark_instance_extension_migration_graph_applied(&db_path, &prepared).unwrap();

        let error =
            finalize_instance_extension_migration_with_remove(&db_path, &graph_applied, |path| {
                nestweaver_store::durable_sidecar::remove_file_durable_if_exists(path)?;
                Err(std::io::Error::other(
                    "injected parent sync failure after real journal unlink",
                ))
            })
            .unwrap_err();
        assert!(error.to_string().contains("after real journal unlink"));
        assert!(instance_extension_migration_journal_path(&db_path).exists());
        let transformed = load_extensions(&db_path);
        assert!(!transformed.contains_key(source));
        assert_eq!(
            get_property(&transformed, destination, "nested"),
            Some(&serde_json::json!({"retry": [true, {"depth": 4}]}))
        );

        let retried = pending_instance_extension_migration(&db_path).unwrap();
        assert!(retried.graph_applied());
        finalize_instance_extension_migration(&db_path, &retried).unwrap();
        assert!(!instance_extension_migration_journal_path(&db_path).exists());
    }
}
