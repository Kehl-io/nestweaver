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

/// Removes the marker when dropped. Hold it for the duration of the pass.
#[must_use = "the marker is removed when the guard drops"]
pub struct EmbedPassGuard {
    path: PathBuf,
}

/// Durably record that a pass is starting. A failure is logged and yields a
/// guard anyway: losing the disclosure must not refuse the embedding work.
pub fn begin(db_path: &Path, record: &EmbedPassRecord) -> EmbedPassGuard {
    let path = marker_path(db_path);
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
    EmbedPassGuard { path }
}

impl Drop for EmbedPassGuard {
    fn drop(&mut self) {
        if let Err(error) =
            nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&self.path)
        {
            tracing::warn!(
                path = %self.path.display(),
                "could not remove embedding pass marker: {error}"
            );
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
