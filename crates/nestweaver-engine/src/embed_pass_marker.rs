//! `<db>.embed-pass.json`: durable evidence that an embedding pass was running.
//!
//! A daemon embedding pass keeps its vectors in memory and checkpoints them to
//! the sidecar only every few minutes (`EMBED_CHECKPOINT_INTERVAL`), because
//! each save rewrites the whole index. A process killed mid-pass therefore
//! loses the work since the last checkpoint, and after the restart nothing
//! said so: `embedding_status.state` read `ready` with no degradation, so a
//! partially embedded brain looked finished.
//!
//! The pass writes this marker when it starts and removes it when it ends by
//! any route the process survives (success, error, panic unwinding). A marker
//! that is present while no pass is running in this process was left by a pass
//! that died, and `brain status` discloses it until a pass completes.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const SUFFIX: &str = ".embed-pass.json";

/// What an interrupted pass had set out to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedPassRecord {
    /// The pass scope (`all`, a repo, ...), as `pass_scope` reports it.
    pub scope: String,
    /// Unix seconds when the pass started.
    pub started_at: u64,
    /// Items the pass planned to embed.
    pub planned: u64,
    /// Items already embedded when the pass started.
    pub already_embedded: u64,
    /// The process that ran the pass (diagnostic only).
    pub pid: u32,
}

/// Path of the marker for `db_path`.
pub fn marker_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, SUFFIX)
}

/// Read a left-behind marker. `None` when absent or unreadable: this is a
/// disclosure, and a corrupt marker is no evidence either way.
pub fn read(db_path: &Path) -> Option<EmbedPassRecord> {
    let bytes = std::fs::read(marker_path(db_path)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether a pass over `finishing` re-embeds everything a pass over `marked`
/// set out to: `all` covers every scope, otherwise the scopes must be equal.
pub fn scope_covers(finishing: &str, marked: &str) -> bool {
    finishing == "all" || finishing == marked
}

/// Removes the marker when dropped, if this pass owns it. Hold it for the
/// duration of the pass.
#[must_use = "the marker is removed when the guard drops"]
pub struct EmbedPassGuard {
    path: PathBuf,
    owns_marker: bool,
}

/// Durably record that a pass is starting. A failure is logged and yields a
/// guard anyway: losing the disclosure must not refuse the embedding work.
///
/// A marker left by an interrupted pass whose scope this pass does not cover
/// (an `all` pass, under a `symbols` pass) is kept as it is: finishing the
/// narrower pass does not finish the wider one, so its gap stays disclosed.
pub fn begin(db_path: &Path, record: &EmbedPassRecord) -> EmbedPassGuard {
    let path = marker_path(db_path);
    if let Some(existing) = read(db_path)
        && !scope_covers(&record.scope, &existing.scope)
    {
        return EmbedPassGuard {
            path,
            owns_marker: false,
        };
    }
    let written = serde_json::to_vec(record)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| {
                file.write_all(&bytes)
            })
        });
    if let Err(error) = written {
        tracing::warn!(path = %path.display(), "could not record embedding pass marker: {error}");
    }
    EmbedPassGuard {
        path,
        owns_marker: true,
    }
}

/// A pass over `scope` that found nothing left to embed completes whatever an
/// interrupted pass it covers had set out to do: retire that pass's marker.
/// A marker for a scope this pass does not cover stays.
pub fn retire_if_covered(db_path: &Path, scope: &str) {
    let Some(existing) = read(db_path) else {
        return;
    };
    if !scope_covers(scope, &existing.scope) {
        return;
    }
    remove(&marker_path(db_path));
}

fn remove(path: &Path) {
    if let Err(error) = nestweaver_store::durable_sidecar::remove_file_durable_if_exists(path) {
        tracing::warn!(
            path = %path.display(),
            "could not remove embedding pass marker: {error}"
        );
    }
}

impl Drop for EmbedPassGuard {
    fn drop(&mut self) {
        if self.owns_marker {
            remove(&self.path);
        }
    }
}

/// Status JSON for a marker left by a pass that is not running now, or
/// `None`. `pass_active` is this process's own view: a running pass owns the
/// marker, so only an inactive one can be reporting a dead pass.
pub fn interrupted_pass_json(
    db_path: &Path,
    pass_active: bool,
    index_live: u64,
) -> Option<serde_json::Value> {
    if pass_active {
        return None;
    }
    let record = read(db_path)?;
    let expected = record.already_embedded.saturating_add(record.planned);
    Some(serde_json::json!({
        "scope": record.scope,
        "started_at": record.started_at,
        "planned": record.planned,
        "already_embedded": record.already_embedded,
        "expected_live": expected,
        "index_live": index_live,
        "message": format!(
            "an embedding pass (scope {}) was interrupted before it finished: {} of about {} \
             vectors are stored, and work since its last checkpoint was lost. It does not resume \
             on its own; run `nestweaver embed` to finish it (already embedded items are skipped)",
            record.scope, index_live, expected
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> EmbedPassRecord {
        EmbedPassRecord {
            scope: "all".to_string(),
            started_at: 1,
            planned: 100,
            already_embedded: 20,
            pid: 7,
        }
    }

    #[test]
    fn a_completed_pass_leaves_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        {
            let _guard = begin(&db, &record());
            assert_eq!(read(&db), Some(record()));
            // While this process runs the pass, nothing is "interrupted".
            assert!(interrupted_pass_json(&db, true, 30).is_none());
        }
        assert!(read(&db).is_none());
        assert!(interrupted_pass_json(&db, false, 30).is_none());
    }

    fn scoped(scope: &str) -> EmbedPassRecord {
        EmbedPassRecord {
            scope: scope.to_string(),
            ..record()
        }
    }

    /// Review: a narrower pass must not retire a wider interrupted pass's
    /// marker, and a covering pass with nothing left to embed must.
    #[test]
    fn only_a_covering_pass_retires_an_interrupted_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        std::mem::forget(begin(&db, &scoped("all")));

        // A `symbols` pass runs and finishes: the `all` gap stays.
        drop(begin(&db, &scoped("symbols")));
        assert_eq!(read(&db).map(|r| r.scope), Some("all".to_string()));
        retire_if_covered(&db, "notes");
        assert!(read(&db).is_some());

        // An `all` pass with nothing left to embed never calls `begin`.
        retire_if_covered(&db, "all");
        assert!(read(&db).is_none());

        // A same-scope pass replaces and then removes the marker.
        std::mem::forget(begin(&db, &scoped("notes")));
        drop(begin(&db, &scoped("notes")));
        assert!(read(&db).is_none());
    }

    #[test]
    fn a_pass_that_died_is_disclosed_with_its_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        // A killed process never runs the guard's Drop.
        std::mem::forget(begin(&db, &record()));
        let disclosed = interrupted_pass_json(&db, false, 45).expect("disclosed");
        assert_eq!(disclosed["expected_live"], 120);
        assert_eq!(disclosed["index_live"], 45);
        assert!(
            disclosed["message"]
                .as_str()
                .unwrap()
                .contains("nestweaver embed")
        );
    }
}
