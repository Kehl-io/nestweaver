//! nw-198 — structural guard on who may drain the regex trigram outbox.
//!
//! The defect this fixes was not a missing call. Invalidation was already
//! universal and transactional (the store's own write path marks a scope dirty
//! inside the mutating transaction), and the reconciler was already a correct
//! idempotent control loop. What was missing was an OWNER: the reconciler ran
//! only as a side effect of two write handlers, so every other mutation path —
//! the vault refresh and both file watchers — enqueued work nobody drained.
//!
//! Adding the missing calls would have been a patch: it leaves each writer
//! responsible for remembering to reconcile, and the next write path added
//! reintroduces the identical bug. That is exactly how the watcher regressed
//! after the background worker was fixed.
//!
//! So the invariant is enforced here rather than left to review: background
//! maintenance has ONE owner, and a writer may only refresh when a caller
//! explicitly asked for it.

use std::path::Path;

/// Source files allowed to mention the reconcile entry points at all.
const ALLOWED: &[&str] = &[
    // The store defines them.
    "crates/nestweaver-store/src/regex.rs",
    // The reconcile loop is the sole background owner, and the IndexRepo
    // handler still honours an EXPLICIT --with-trigrams/--rebuild-trigrams
    // (the `refresh=wait_for` equivalent for CI and scripted reindex).
    "crates/nestweaver-daemon/src/server.rs",
    // The direct (--local / --no-daemon) path has no daemon, so no reconcile
    // loop exists to drain for it; it must still refresh inline.
    "src/main.rs",
    // Test fixture.
    "crates/nestweaver-engine/src/index.rs",
];

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name == ".git" {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_the_reconciler_and_explicit_requests_may_refresh_trigrams() {
    let root = repo_root();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);
    rust_sources(&root.join("src"), &mut sources);

    let mut offenders = Vec::new();
    for path in sources {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.contains(&rel.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            // Skip prose: the bug is well-documented in comments and those
            // must stay readable.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if line.contains("refresh_trigram_index(") || line.contains("rebuild_trigram_index(") {
                offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "nw-198: background trigram maintenance has exactly one owner — the daemon's \
         reconcile loop. A writer must only enqueue (the store's write path already does \
         that transactionally) and must never drain the outbox itself; draining from a \
         writer is what left the vault path and both watchers permanently stale. If a new \
         call site is genuinely an EXPLICIT user request (not background maintenance), add \
         its file to ALLOWED with a comment saying why.\n  {}",
        offenders.join("\n  ")
    );
}

/// The watcher is the specific path that regressed last time: it was fixed for
/// the background worker and left broken for file watching. Pin it explicitly
/// so a future "just refresh here too" cannot land quietly.
#[test]
fn the_watchers_never_touch_trigrams() {
    for rel in [
        "crates/nestweaver-engine/src/watcher.rs",
        "crates/nestweaver-engine/src/watch_code.rs",
    ] {
        let text = std::fs::read_to_string(repo_root().join(rel))
            .unwrap_or_else(|e| panic!("{rel} must be readable: {e}"));
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            assert!(
                !line.contains("trigram_index("),
                "{rel}:{}: a watcher must only mutate — its writes already enqueue outbox \
                 work, and the reconcile loop drains it. Refreshing per watcher batch would \
                 run a ~50% duty cycle on save-heavy work: the refresh cost is dominated by \
                 fixed overhead (measured 918ms for 2 changed nodes), while the watcher \
                 debounces into ~2s batches.",
                n + 1
            );
        }
    }
}
