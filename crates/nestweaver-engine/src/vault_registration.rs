//! nw-587. A record of which vaults a database has been asked to hold, kept
//! OUTSIDE the graph so it survives losing the graph's un-checkpointed tail.
//!
//! The documented WAL-corruption recovery (`wal_corruption_runbook` in the
//! CLI) moves the log aside and reopens the database. Whatever the log held
//! and the main file did not is gone — and in the incident that filed this,
//! that was the vault publication while the code repos had already been
//! checkpointed. Afterwards `brain list` printed `No vaults indexed` and exited
//! 0: a bare empty list is indistinguishable from a brain that never had a
//! vault, so nothing told the operator what the recovery had cost.
//!
//! The graph cannot testify about its own lost tail, so the evidence has to
//! live beside it. `<db>.vault-registrations.json` is written after every
//! successful vault publication and is not one of the five artifacts the
//! runbook moves. `brain status` / `brain list` compare it with the vaults the
//! graph actually holds and NAME each one that is registered but absent,
//! together with the `brain add` command that restores it.
//!
//! Deliberately a disclosure, not an automatic re-add: re-indexing a vault is
//! a write that takes minutes on a real brain, and a read-only `status` must
//! not start one. The remedy is printed; the operator runs it.
//!
//! Keeping the record honest — a vault that was removed ON PURPOSE must not be
//! reported as lost:
//!
//!   * `remove_vault` (every `brain remove` / `brain_remove_source` route ends
//!     there) and `prune_stale` forget the uid and every registration at its
//!     root, and `brain remove <path>` forgets a registration whose graph row
//!     is already gone.
//!   * A registration also counts as present when a live vault has the same
//!     ROOT PATH under another uid, which is what `instance merge` produces.
//!   * A registration whose root directory no longer exists is not reported:
//!     that is what `prune_stale` removes, and a vault that cannot be re-added
//!     has no remedy to offer.
//!
//! A database indexed before this file existed has no registrations, so it
//! reports nothing missing until its vaults are next added or refreshed — a
//! refresh that finds no changed notes registers too.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Sidecar suffix, appended to the database path.
pub const VAULT_REGISTRATIONS_SUFFIX: &str = ".vault-registrations.json";

const REGISTRY_VERSION: u32 = 1;

/// One vault the database was asked to hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRegistration {
    pub uid: String,
    pub name: String,
    pub root_path: String,
    pub instance_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    version: u32,
    vaults: Vec<VaultRegistration>,
}

/// `<db>.vault-registrations.json`.
pub fn registrations_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, VAULT_REGISTRATIONS_SUFFIX)
}

fn load(db_path: &Path) -> anyhow::Result<Registry> {
    let path = registrations_path(db_path);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "vault registrations {} are unreadable: {error}",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(anyhow::anyhow!(
            "read vault registrations {}: {error}",
            path.display()
        )),
    }
}

fn save(db_path: &Path, registry: &Registry) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(registry)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(&registrations_path(db_path), |file| {
        file.write_all(&bytes)
    })?;
    Ok(())
}

/// Every registration recorded for `db_path` (empty when none were).
pub fn registrations(db_path: &Path) -> anyhow::Result<Vec<VaultRegistration>> {
    Ok(load(db_path)?.vaults)
}

/// Record (or refresh) a vault after its publication committed. Idempotent:
/// an unchanged registration does not rewrite the file.
///
/// One root holds ONE registration: an entry at the same canonical root under
/// another uid (what `instance merge` leaves) is replaced, not kept beside it.
///
/// An unreadable sidecar is REWRITTEN here rather than failing every record
/// forever: the entries it held cannot be recovered by refusing, and until
/// this rewrite `missing` keeps disclosing `vault_registrations_unreadable`.
///
/// Load-modify-save is not locked against a concurrent writer. Graph writers
/// hold the write lease while publishing, which serialises records; a
/// `forget` racing a record can lose one update, and the worst outcome is one
/// stale or missing disclosure until the next add, refresh, or remove.
pub fn record(db_path: &Path, vault: &nestweaver_schema::Vault) -> anyhow::Result<()> {
    let mut registry = load(db_path).unwrap_or_else(|error| {
        tracing::warn!("nw-587: rewriting unreadable vault registrations: {error:#}");
        Registry::default()
    });
    let entry = VaultRegistration {
        uid: vault.uid.clone(),
        name: vault.name.clone(),
        root_path: vault.root_path.clone(),
        instance_id: vault.instance_id.clone(),
    };
    let root = canonical(Path::new(&entry.root_path));
    let same_root =
        |existing: &VaultRegistration| canonical(Path::new(&existing.root_path)) == root;
    let mut at_root = registry
        .vaults
        .iter()
        .filter(|existing| same_root(existing));
    if at_root.next() == Some(&entry) && at_root.next().is_none() {
        return Ok(());
    }
    registry
        .vaults
        .retain(|existing| existing.uid != entry.uid && !same_root(existing));
    registry.vaults.push(entry);
    registry.version = REGISTRY_VERSION;
    save(db_path, &registry)
}

/// [`record`] for a store that knows its path; in-memory stores have no
/// sidecar and record nothing. Best-effort: the graph write already committed,
/// so a failure here is logged rather than failing the index.
pub(crate) fn record_for_store(
    store: &nestweaver_store::GraphStore,
    vault: &nestweaver_schema::Vault,
) {
    if let Some(db_path) = store.db_path()
        && let Err(error) = record(db_path, vault)
    {
        tracing::warn!(
            "nw-587: failed to record vault registration {}: {error:#}",
            vault.uid
        );
    }
}

/// nw-608: refuse to register a NEW vault under a name another vault at a
/// different root already holds.
///
/// A vault's uid is derived from its root path, so registering `r4-vault` at a
/// new root (a copied database watched from a new checkout) did not move the
/// existing vault — it created a second one with the same name. `brain search`
/// then returned both vaults' notes, and every name selector (`brain remove
/// <name>`, `brain_remove_source`) became ambiguous.
///
/// Refused rather than rehomed: a uid, and every note/heading/section/tag uid
/// beneath it, is a hash of the root, so there is no primitive that moves a
/// vault — "rehoming" would be a remove plus a full re-index, a destructive
/// write the operator should choose, not one a watch should perform.
///
/// Scope, deliberately narrow so existing states keep working:
///   * a vault whose uid is already in the graph refreshes in place, even in a
///     database that was forked before this guard existed;
///   * another vault at the SAME root (a different instance) is nw-098's case,
///     guarded by the CLI and repaired by `instance merge`, not this one.
///
/// Every local vault publication calls this — the full and `--since` index
/// routes and the watcher's startup — so `brain add`, `brain refresh`,
/// `brain watch`, the daemon RPCs and MCP `brain_add_source` share one answer.
pub fn refuse_duplicate_vault_name(
    store: &nestweaver_store::GraphStore,
    uid: &str,
    name: &str,
    root: &Path,
) -> anyhow::Result<()> {
    let vaults = store.list_vaults(None)?;
    if vaults.iter().any(|vault| vault.uid == uid) {
        return Ok(());
    }
    // Compared the way vault SELECTORS compare names (case-insensitively), so
    // `Brain` beside `brain` is refused too: a selector could not tell them
    // apart either.
    let wanted = canonical(root);
    let key = crate::node_scope::vault_name_key(name);
    let Some(existing) = vaults.iter().find(|vault| {
        crate::node_scope::vault_name_key(&vault.name) == key
            && canonical(Path::new(&vault.root_path)) != wanted
    }) else {
        return Ok(());
    };
    // The remedy pins the EXISTING vault's instance: `list_vaults(None)` spans
    // instances, and an unpinned `brain remove` resolves the caller's.
    anyhow::bail!(
        "a vault named '{}' is already indexed at {} ({}); refusing to register a \
         second vault named '{name}' at {} — both would answer searches and \
         selecting the vault by name would be ambiguous.\n\
         help: pass a different --name, or if the vault moved, remove the old one first \
         (`nestweaver brain remove {} --instance {}`) and add it at the new path.",
        existing.name,
        existing.root_path,
        existing.uid,
        root.display(),
        crate::shell_quote(&existing.root_path),
        crate::shell_quote(&existing.instance_id),
    )
}

/// Forget registrations matching `predicate`. Returns how many were dropped.
fn forget_where(
    db_path: &Path,
    predicate: impl Fn(&VaultRegistration) -> bool,
) -> anyhow::Result<usize> {
    let mut registry = load(db_path)?;
    let before = registry.vaults.len();
    registry.vaults.retain(|entry| !predicate(entry));
    let removed = before - registry.vaults.len();
    if removed > 0 {
        registry.version = REGISTRY_VERSION;
        save(db_path, &registry)?;
    }
    Ok(removed)
}

/// Forget a deliberately removed vault by uid.
pub fn forget_uid(db_path: &Path, uid: &str) -> anyhow::Result<usize> {
    forget_where(db_path, |entry| entry.uid == uid)
}

/// Forget a deliberately removed vault: its uid AND every registration at its
/// root. After `instance merge` one root may carry a pre-merge uid too, and
/// forgetting only the removed uid would report that root as lost.
pub fn forget_vault(db_path: &Path, uid: &str, root_path: &str) -> anyhow::Result<usize> {
    let root = canonical(Path::new(root_path));
    forget_where(db_path, |entry| {
        entry.uid == uid || canonical(Path::new(&entry.root_path)) == root
    })
}

/// Forget every registration rooted at `root` (compared canonically).
pub fn forget_root(db_path: &Path, root: &Path) -> anyhow::Result<usize> {
    let wanted = canonical(root);
    forget_where(db_path, |entry| {
        canonical(Path::new(&entry.root_path)) == wanted
    })
}

/// The form two watched roots are compared in: canonicalized when the path
/// exists, as given when it does not (a removed root still has to match the
/// registration that names it). Public so the daemon's watcher bookkeeping
/// compares roots the same way instead of restating it.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Registrations the graph no longer holds: no live vault has the uid or the
/// root path, and the root directory still exists (see the module docs for
/// why each clause is there). `live` yields `(uid, root_path)` pairs.
pub fn missing<'a>(
    db_path: &Path,
    live: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> anyhow::Result<Vec<VaultRegistration>> {
    let registry = load(db_path)?;
    if registry.vaults.is_empty() {
        return Ok(Vec::new());
    }
    let mut live_uids = std::collections::HashSet::new();
    let mut live_roots = std::collections::HashSet::new();
    for (uid, root) in live {
        live_uids.insert(uid.to_string());
        live_roots.insert(canonical(Path::new(root)));
    }
    Ok(registry
        .vaults
        .into_iter()
        .filter(|entry| !live_uids.contains(&entry.uid))
        .filter(|entry| !live_roots.contains(&canonical(Path::new(&entry.root_path))))
        .filter(|entry| Path::new(&entry.root_path).is_dir())
        .collect())
}

/// The exact command that re-registers `entry` under the same identity.
pub fn readd_command(db_path: &Path, entry: &VaultRegistration) -> String {
    let mut command = format!(
        "nestweaver brain add {} --db {}",
        crate::shell_quote(&entry.root_path),
        crate::shell_quote(&db_path.to_string_lossy()),
    );
    if entry.instance_id != "default" {
        command.push_str(&format!(
            " --instance {}",
            crate::shell_quote(&entry.instance_id)
        ));
    }
    let basename = Path::new(&entry.root_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    if basename.as_deref() != Some(entry.name.as_str()) {
        command.push_str(&format!(" --name {}", crate::shell_quote(&entry.name)));
    }
    command
}

/// The one sentence every surface prints for a missing registration.
pub fn missing_warning(entry: &VaultRegistration) -> String {
    format!(
        "vault '{}' at {} was registered in this database but is not in the graph \
         (a WAL move-aside or other recovery dropped it); its notes are not searchable \
         until it is re-added. If it was dropped on purpose, `nestweaver brain remove {}` \
         forgets it.",
        entry.name,
        entry.root_path,
        crate::shell_quote(&entry.root_path),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(uid: &str, root: &Path) -> nestweaver_schema::Vault {
        nestweaver_schema::Vault {
            uid: uid.into(),
            name: root.file_name().unwrap().to_string_lossy().into_owned(),
            root_path: root.to_string_lossy().into_owned(),
            instance_id: "default".into(),
        }
    }

    #[test]
    fn a_registered_vault_absent_from_the_graph_is_missing_and_a_live_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let lost = dir.path().join("lost-vault");
        let kept = dir.path().join("kept-vault");
        std::fs::create_dir_all(&lost).unwrap();
        std::fs::create_dir_all(&kept).unwrap();
        record(&db, &vault("vlt:default:lost", &lost)).unwrap();
        record(&db, &vault("vlt:default:kept", &kept)).unwrap();
        let kept_root = kept.to_string_lossy().into_owned();

        let missing = missing(&db, [("vlt:default:kept", kept_root.as_str())]).unwrap();
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].uid, "vlt:default:lost");
        let command = readd_command(&db, &missing[0]);
        assert!(
            command.starts_with("nestweaver brain add ") && command.contains("lost-vault"),
            "{command}"
        );
        assert!(
            !command.contains("--instance") && !command.contains("--name"),
            "{command}"
        );
    }

    #[test]
    fn same_root_under_another_uid_removed_uid_and_vanished_root_are_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let merged = dir.path().join("merged");
        let removed = dir.path().join("removed");
        std::fs::create_dir_all(&merged).unwrap();
        std::fs::create_dir_all(&removed).unwrap();
        record(&db, &vault("vlt:old:merged", &merged)).unwrap();
        record(&db, &vault("vlt:default:removed", &removed)).unwrap();
        record(&db, &vault("vlt:default:gone", &dir.path().join("gone"))).unwrap();
        assert_eq!(forget_uid(&db, "vlt:default:removed").unwrap(), 1);

        let merged_root = merged.to_string_lossy().into_owned();
        let missing = missing(&db, [("vlt:new:merged", merged_root.as_str())]).unwrap();
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// Review fix 1: after `instance merge` one root carried two uids — the
    /// old one never forgotten — so removing the live uid left the old one
    /// behind and reported the root as lost.
    #[test]
    fn one_root_holds_one_registration_and_removal_forgets_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let root = dir.path().join("merged");
        std::fs::create_dir_all(&root).unwrap();
        record(&db, &vault("vlt:old:merged", &root)).unwrap();
        record(&db, &vault("vlt:new:merged", &root)).unwrap();
        let entries = registrations(&db).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].uid, "vlt:new:merged");

        // A sidecar written before this fix can still hold both.
        let both = serde_json::json!({"version": 1, "vaults": [
            {"uid": "vlt:old:merged", "name": "merged", "root_path": root, "instance_id": "old"},
            {"uid": "vlt:new:merged", "name": "merged", "root_path": root, "instance_id": "new"},
        ]});
        std::fs::write(registrations_path(&db), both.to_string()).unwrap();
        assert_eq!(
            forget_vault(&db, "vlt:new:merged", &root.to_string_lossy()).unwrap(),
            2
        );
        assert!(missing(&db, std::iter::empty()).unwrap().is_empty());
    }

    /// Review fix 2: an unreadable sidecar made every record/forget fail
    /// forever. The next record rewrites it; until then `missing` says so.
    #[test]
    fn an_unreadable_sidecar_is_disclosed_until_the_next_record_rewrites_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let root = dir.path().join("v");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(registrations_path(&db), b"{not json").unwrap();
        assert!(missing(&db, std::iter::empty()).is_err());
        record(&db, &vault("vlt:default:v", &root)).unwrap();
        assert_eq!(registrations(&db).unwrap().len(), 1);
        assert!(missing(&db, std::iter::empty()).unwrap().len() == 1);
    }

    /// nw-608 review: vault selectors match names case-insensitively
    /// (`node_scope::resolve_vault_filter`), so `Brain` beside `brain` is
    /// just as ambiguous as an exact duplicate. And the hint must name the
    /// existing vault's instance, or `brain remove` targets the wrong one.
    #[test]
    fn a_case_variant_name_is_refused_and_the_hint_pins_the_existing_instance() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a");
        let copy = dir.path().join("b");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&copy).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        store
            .insert_vault(&nestweaver_schema::Vault {
                uid: "vlt:work:a".into(),
                name: "brain".into(),
                root_path: original.to_string_lossy().into_owned(),
                instance_id: "work".into(),
            })
            .unwrap();

        let message = format!(
            "{:#}",
            refuse_duplicate_vault_name(&store, "vlt:default:b", "Brain", &copy).unwrap_err()
        );
        assert!(message.contains("--instance work"), "{message}");
        assert!(message.contains("brain remove"), "{message}");

        // Counterweight: a genuinely different name passes.
        refuse_duplicate_vault_name(&store, "vlt:default:b", "brain-copy", &copy).unwrap();
    }

    #[test]
    fn no_sidecar_reports_nothing_and_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        assert!(missing(&db, std::iter::empty()).unwrap().is_empty());
        let root = dir.path().join("v");
        std::fs::create_dir_all(&root).unwrap();
        record(&db, &vault("vlt:default:v", &root)).unwrap();
        record(&db, &vault("vlt:default:v", &root)).unwrap();
        assert_eq!(registrations(&db).unwrap().len(), 1);
        assert_eq!(forget_root(&db, &root).unwrap(), 1);
        assert!(registrations(&db).unwrap().is_empty());
    }
}
