//! Feature F12 — git-activity-dampened CodeRank.
//!
//! Demotes dormant code at rank-*read* time (not inside the PPR fixpoint) using
//! git-history recency, so an actively-developed file outranks a stale one of
//! the same name. The PPR algorithm in `nestweaver-algorithms` stays pure and
//! WASM-compatible; the recency signal is mined here, persisted as a sidecar
//! (`<db>.gitactivity.json`, a `path -> score` map), loaded into the store on
//! open (mirroring the PageRank / interaction caches), and applied as a
//! per-file multiplier wherever `pagerank_score` is *consumed*
//! (`nestweaver-store::ranking` and `nestweaver-engine::hubs`).
//!
//! ## The multiplier
//!
//! ```text
//! effective = pagerank * clamp(1 + activity_weight * (score - 0.5), 0.4, 1.6)
//! ```
//!
//! A file with no recency score → neutral (multiplier `1.0`).
//!
//! ## Clamp / weight rationale (RFC bug fix)
//!
//! With `score ∈ [0, 1]`, the factor `1 + w*(score - 0.5)` spans
//! `[1 - w/2, 1 + w/2]`. The RFC quoted a `[0.4, 1.6]` clamp but a weight of
//! `0.6`, which only reaches `[0.7, 1.3]` — the clamp would never bind and the
//! intended demotion strength is halved. To actually span the full `[0.4, 1.6]`
//! range the weight must be `1.2` (so `±w/2 = ±0.6`). We therefore default
//! `activity_weight = 1.2`. The clamp is still applied so out-of-range scores
//! (or larger configured weights) cannot push the multiplier outside
//! `[0.4, 1.6]`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Default activity weight. See the module-level "Clamp / weight rationale":
/// `1.2` (not the RFC's `0.6`) is required for the factor to actually span the
/// intended `[0.4, 1.6]` clamp range.
pub const DEFAULT_GIT_ACTIVITY_WEIGHT: f64 = 1.2;

/// Lower clamp bound for the rank-read multiplier.
pub const GIT_ACTIVITY_MULT_MIN: f64 = 0.4;
/// Upper clamp bound for the rank-read multiplier.
pub const GIT_ACTIVITY_MULT_MAX: f64 = 1.6;

/// Number of most-recent non-bulk touching commits averaged per file.
const RECENCY_WINDOW: usize = 20;

/// Decay half-life-ish constant (in days) for `exp(-Δdays / TAU)`.
const DECAY_TAU_DAYS: f64 = 180.0;

/// Bulk-commit detection: commits whose touched-file count falls in the top
/// decile (≥ 90th percentile) of the per-repo distribution are treated as bulk
/// (refactors, vendoring, formatting sweeps) and skipped. Using a *per-repo*
/// percentile rather than a fixed threshold (e.g. 500) adapts to repos of any
/// size.
const BULK_PERCENTILE: f64 = 0.90;

/// Seconds per day, for converting an epoch delta to days.
const SECS_PER_DAY: f64 = 86_400.0;

/// One commit's touch record: its author date (epoch seconds) and the set of
/// repo-relative file paths it modified. This is the pure unit the recency
/// math operates on, so tests can build synthetic histories without git.
#[derive(Debug, Clone)]
pub struct CommitTouch {
    /// Author date as Unix epoch seconds (parsed from `git log %aI`).
    pub author_epoch: f64,
    /// Repo-relative file paths touched by this commit.
    pub files: Vec<String>,
}

/// Compute the bulk-commit cutoff: the file-count at the `BULK_PERCENTILE`
/// position of the distribution. Commits touching *more* files than this are
/// considered bulk. Returns `usize::MAX` when there are too few commits to
/// meaningfully distinguish bulk from normal (so nothing is skipped).
fn bulk_file_count_cutoff(commits: &[CommitTouch]) -> usize {
    // Need a reasonable sample before a percentile is meaningful.
    if commits.len() < 10 {
        return usize::MAX;
    }
    let mut counts: Vec<usize> = commits.iter().map(|c| c.files.len()).collect();
    counts.sort_unstable();
    // Index at the 90th percentile (nearest-rank). Commits strictly above this
    // value are skipped, so a uniform distribution keeps ~the top decile out.
    let idx = ((counts.len() as f64) * BULK_PERCENTILE).ceil() as usize;
    let idx = idx.saturating_sub(1).min(counts.len() - 1);
    counts[idx]
}

/// Pure recency scorer. Given a commit history (newest-first or any order),
/// "now" as epoch seconds, and the bulk-commit cutoff, produce a
/// `path -> score ∈ [0, 1]` map.
///
/// For each file we average `exp(-Δdays / TAU)` over the most-recent
/// [`RECENCY_WINDOW`] *non-bulk* commits that touch it (Δdays measured from
/// `now` back to the commit author date). A file touched only by bulk commits
/// has no contributing commits and is therefore excluded from the map (so it
/// gets the neutral multiplier at read time, not a demotion — bulk activity is
/// neither evidence of liveness nor of dormancy).
pub fn compute_recency_scores(
    commits: &[CommitTouch],
    now_epoch: f64,
    bulk_cutoff: usize,
) -> HashMap<String, f64> {
    // Per-file list of recency contributions, newest-first.
    let mut per_file: HashMap<String, Vec<f64>> = HashMap::new();

    // Sort commits newest-first so the per-file window keeps the latest touches.
    let mut sorted: Vec<&CommitTouch> = commits.iter().collect();
    sorted.sort_by(|a, b| {
        b.author_epoch
            .partial_cmp(&a.author_epoch)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for commit in sorted {
        // Skip bulk commits — they carry no per-file liveness signal.
        if commit.files.len() > bulk_cutoff {
            continue;
        }
        let delta_days = ((now_epoch - commit.author_epoch) / SECS_PER_DAY).max(0.0);
        let decay = (-delta_days / DECAY_TAU_DAYS).exp();
        for path in &commit.files {
            let entry = per_file.entry(path.clone()).or_default();
            if entry.len() < RECENCY_WINDOW {
                entry.push(decay);
            }
        }
    }

    per_file
        .into_iter()
        .map(|(path, decays)| {
            let mean = decays.iter().sum::<f64>() / decays.len() as f64;
            (path, mean.clamp(0.0, 1.0))
        })
        .collect()
}

/// Convenience wrapper: compute the per-repo bulk cutoff from `commits`, then
/// score. This is the entry point the miner uses after running `git log`.
pub fn score_commits(commits: &[CommitTouch], now_epoch: f64) -> HashMap<String, f64> {
    let cutoff = bulk_file_count_cutoff(commits);
    compute_recency_scores(commits, now_epoch, cutoff)
}

/// The rank-read multiplier applied to a `pagerank_score`.
///
/// - `score == None` → neutral `1.0` (no recency data for this file).
/// - `score == Some(s)` → `clamp(1 + weight * (s - 0.5), 0.4, 1.6)`.
///
/// The result is always within `[0.4, 1.6]`.
pub fn git_activity_multiplier(score: Option<f64>, weight: f64) -> f64 {
    match score {
        None => 1.0,
        Some(s) => (1.0 + weight * (s - 0.5)).clamp(GIT_ACTIVITY_MULT_MIN, GIT_ACTIVITY_MULT_MAX),
    }
}

/// Run `git log` over `repo_path` and parse it into [`CommitTouch`] records.
///
/// Uses `--name-only` with a `%H\t%aI` header line per commit (author date,
/// `%aI` = strict ISO 8601) and `--no-merges` to skip merge commits. Returns an
/// empty vec on any git failure (not a git repo, git missing, etc.) so callers
/// degrade to the neutral / no-sidecar path.
pub fn mine_commits(repo_path: &Path) -> Vec<CommitTouch> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "log",
            "--no-merges",
            "--name-only",
            "--pretty=format:\x01%H\t%aI",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!(
                "git log failed in {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&o.stderr)
            );
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!("could not run git in {}: {e}", repo_path.display());
            return Vec::new();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_git_log(&stdout)
}

/// Parse the textual output of the `git log` invocation in [`mine_commits`].
///
/// Each commit starts with a `\x01`-prefixed header `\x01<sha>\t<author-iso>`
/// followed by zero or more file-path lines until the next header or EOF.
/// Pulled out as a free function so it is unit-testable without invoking git.
pub fn parse_git_log(stdout: &str) -> Vec<CommitTouch> {
    let mut commits: Vec<CommitTouch> = Vec::new();
    let mut current: Option<CommitTouch> = None;

    for line in stdout.lines() {
        if let Some(header) = line.strip_prefix('\x01') {
            // Flush the previous commit.
            if let Some(c) = current.take() {
                commits.push(c);
            }
            // Header is `<sha>\t<author-iso>`. We only need the date.
            let author_iso = header.split('\t').nth(1).unwrap_or("");
            let author_epoch = crate::recency::parse_iso8601_to_epoch(author_iso);
            current = Some(CommitTouch {
                author_epoch,
                files: Vec::new(),
            });
        } else if !line.trim().is_empty()
            && let Some(c) = current.as_mut()
        {
            c.files.push(line.trim().to_string());
        }
    }
    if let Some(c) = current.take() {
        commits.push(c);
    }
    commits
}

/// Current wall-clock time as Unix epoch seconds (f64).
fn now_epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Mine `repo_path` and produce the `path -> score` recency map written to the
/// `<db>.gitactivity.json` sidecar. Returns an empty map when the repo has no
/// usable git history.
pub fn compute_git_activity(repo_path: &Path) -> HashMap<String, f64> {
    let commits = mine_commits(repo_path);
    if commits.is_empty() {
        return HashMap::new();
    }
    score_commits(&commits, now_epoch_secs())
}

pub use nestweaver_store::git_activity_sidecar::{GITACTIVITY_VERSION, GitActivitySidecar};

/// Replace ONE repo's slice, conserving every other repo's.
///
/// The old implementation was a plain `fs::write` of just the incoming repo,
/// which is what erased the rest. This mirrors `save_filemeta_for_repo`:
/// load, replace this repo's entry, write atomically. Replace-not-merge within
/// the repo is deliberate — a re-index recomputes that repo's scores in full,
/// so stale paths must not survive.
pub fn save_git_activity_for_repo(
    repo_uid: &str,
    scores: &HashMap<String, f64>,
    path: &Path,
) -> Result<(), anyhow::Error> {
    let mut sidecar = load_git_activity_sidecar(path);
    sidecar.repos.insert(repo_uid.to_string(), scores.clone());
    save_git_activity_sidecar(&sidecar, path)
}

/// Write the sidecar atomically. The previous `fs::write` was not atomic, so a
/// crash mid-write left a truncated file — which `load_git_activity_sidecar`
/// then reads as empty, i.e. silently neutral ranking.
pub fn save_git_activity_sidecar(
    sidecar: &GitActivitySidecar,
    path: &Path,
) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string(sidecar)?;
    crate::manifest::atomic_replace_file(path, |file| {
        use std::io::Write;
        file.write_all(json.as_bytes())
    })
}

/// Load the sidecar. Missing, corrupt, or OLD-FORMAT files yield an empty one.
///
/// A v1 flat map fails to deserialize into this shape and is discarded rather
/// than migrated. Migration is not merely unnecessary here, it is UNSOUND: a v1
/// key carries no repo dimension, so attributing it to any repo is the exact
/// mis-attribution the format change exists to prevent. Discarding costs one
/// re-index and is the honest outcome — the same call `FileMetaSidecar` makes.
///
/// Degrading to empty is safe by construction: a missing score yields a
/// multiplier of exactly 1.0, so the ranking reverts to unmodified PageRank
/// rather than to something wrong.
pub fn load_git_activity_sidecar(path: &Path) -> GitActivitySidecar {
    match std::fs::read_to_string(path) {
        Ok(data) => match serde_json::from_str::<GitActivitySidecar>(&data) {
            Ok(sidecar) if sidecar.version == GITACTIVITY_VERSION => sidecar,
            Ok(sidecar) => {
                tracing::debug!(
                    path = %path.display(),
                    found_version = sidecar.version,
                    expected_version = GITACTIVITY_VERSION,
                    "git-activity sidecar version mismatch; discarding (re-index to restore)"
                );
                GitActivitySidecar::default()
            }
            Err(error) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %error,
                    "git-activity sidecar corrupt or legacy v1 flat format; discarding"
                );
                GitActivitySidecar::default()
            }
        },
        Err(_) => GitActivitySidecar::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `days_ago` → epoch seconds relative to a fixed `now`.
    fn at(now: f64, days_ago: f64) -> f64 {
        now - days_ago * SECS_PER_DAY
    }

    #[test]
    fn recent_file_outranks_stale_file() {
        let now = 1_700_000_000.0;
        // `fresh.rs` touched 5 days ago; `stale.rs` touched 400 days ago.
        let commits = vec![
            CommitTouch {
                author_epoch: at(now, 5.0),
                files: vec!["fresh.rs".to_string()],
            },
            CommitTouch {
                author_epoch: at(now, 400.0),
                files: vec!["stale.rs".to_string()],
            },
        ];
        // Too few commits for a percentile → nothing treated as bulk.
        let scores = compute_recency_scores(&commits, now, usize::MAX);

        let fresh = scores["fresh.rs"];
        let stale = scores["stale.rs"];
        assert!(
            fresh > stale,
            "recent file ({fresh:.4}) should outrank stale file ({stale:.4})"
        );
        // exp(-5/180) ≈ 0.973, exp(-400/180) ≈ 0.108.
        assert!((fresh - 0.9726).abs() < 0.01, "fresh score {fresh}");
        assert!((stale - 0.1084).abs() < 0.01, "stale score {stale}");
    }

    #[test]
    fn bulk_only_file_is_excluded() {
        let now = 1_700_000_000.0;
        let mut commits = Vec::new();
        // 15 small commits (1 file each) touching live files — establishes the
        // per-repo distribution where 1-file commits are the norm.
        for i in 0..15 {
            commits.push(CommitTouch {
                author_epoch: at(now, i as f64 + 1.0),
                files: vec![format!("live_{i}.rs")],
            });
        }
        // One huge bulk commit (a formatting sweep) touching 100 files,
        // including `only_bulk.rs` which appears in NO other commit.
        let mut bulk_files: Vec<String> = (0..99).map(|i| format!("swept_{i}.rs")).collect();
        bulk_files.push("only_bulk.rs".to_string());
        commits.push(CommitTouch {
            author_epoch: at(now, 2.0),
            files: bulk_files,
        });

        let cutoff = bulk_file_count_cutoff(&commits);
        // The 100-file commit must be above the cutoff (i.e. detected as bulk).
        assert!(
            100 > cutoff,
            "bulk commit (100 files) should exceed the per-repo cutoff ({cutoff})"
        );

        let scores = compute_recency_scores(&commits, now, cutoff);
        assert!(
            !scores.contains_key("only_bulk.rs"),
            "a file touched only by a bulk commit must be excluded"
        );
        // The genuinely-live files survive.
        assert!(scores.contains_key("live_0.rs"));
    }

    #[test]
    fn window_caps_contributions() {
        let now = 1_700_000_000.0;
        // 30 commits each touching `hot.rs`, oldest last. Only the most recent
        // RECENCY_WINDOW (20) should count, so very old touches don't drag the
        // mean down below what the window implies.
        let mut commits = Vec::new();
        for i in 0..30 {
            commits.push(CommitTouch {
                author_epoch: at(now, i as f64), // 0..29 days ago
                files: vec!["hot.rs".to_string()],
            });
        }
        let scores = compute_recency_scores(&commits, now, usize::MAX);
        // Mean over the 20 most-recent (0..19 days). exp decay of those is high.
        let s = scores["hot.rs"];
        // Mean of exp(-d/180) for d in 0..20 ≈ 0.948.
        assert!((s - 0.948).abs() < 0.02, "windowed mean {s}");
    }

    #[test]
    fn multiplier_is_neutral_when_no_score() {
        let m = git_activity_multiplier(None, DEFAULT_GIT_ACTIVITY_WEIGHT);
        assert!((m - 1.0).abs() < f64::EPSILON, "no score → neutral 1.0");
    }

    #[test]
    fn multiplier_within_clamp_bounds() {
        let w = DEFAULT_GIT_ACTIVITY_WEIGHT; // 1.2
        // score 0.5 → exactly neutral.
        assert!((git_activity_multiplier(Some(0.5), w) - 1.0).abs() < 1e-9);
        // score 1.0 → 1 + 1.2*0.5 = 1.6 (upper bound).
        assert!((git_activity_multiplier(Some(1.0), w) - 1.6).abs() < 1e-9);
        // score 0.0 → 1 - 1.2*0.5 = 0.4 (lower bound).
        assert!((git_activity_multiplier(Some(0.0), w) - 0.4).abs() < 1e-9);
        // Every score stays inside [0.4, 1.6], even with a larger weight.
        for i in 0..=100 {
            let s = i as f64 / 100.0;
            let m = git_activity_multiplier(Some(s), 5.0);
            assert!(
                (GIT_ACTIVITY_MULT_MIN..=GIT_ACTIVITY_MULT_MAX).contains(&m),
                "multiplier {m} for score {s} out of clamp bounds"
            );
        }
    }

    #[test]
    fn weight_fix_reaches_full_clamp_range() {
        // The RFC bug: weight 0.6 only spans [0.7, 1.3], never hitting the
        // [0.4, 1.6] clamp. The corrected default 1.2 spans the full range.
        let buggy = git_activity_multiplier(Some(1.0), 0.6);
        assert!(
            (buggy - 1.3).abs() < 1e-9,
            "w=0.6 tops out at 1.3, got {buggy}"
        );
        let fixed = git_activity_multiplier(Some(1.0), DEFAULT_GIT_ACTIVITY_WEIGHT);
        assert!((fixed - 1.6).abs() < 1e-9, "w=1.2 reaches 1.6, got {fixed}");
    }

    #[test]
    fn parse_git_log_groups_files_under_commits() {
        let log = "\x01abc123\t2024-01-10T12:00:00Z\nsrc/a.rs\nsrc/b.rs\n\x01def456\t2024-01-05T08:00:00Z\nsrc/c.rs\n";
        let commits = parse_git_log(log);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].files, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(commits[1].files, vec!["src/c.rs"]);
        assert!(commits[0].author_epoch > commits[1].author_epoch);
    }

    #[test]
    fn sidecar_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.gitactivity.json");
        let mut scores = HashMap::new();
        scores.insert("src/a.rs".to_string(), 0.8);
        scores.insert("src/b.rs".to_string(), 0.2);
        save_git_activity_for_repo("repo:test", &scores, &path).unwrap();
        let loaded = load_git_activity_sidecar(&path);
        assert_eq!(loaded.repos["repo:test"].len(), 2);
        assert!((loaded.repos["repo:test"]["src/a.rs"] - 0.8).abs() < 1e-9);
    }

    #[test]
    fn load_missing_sidecar_is_empty() {
        let loaded = load_git_activity_sidecar(Path::new("/nonexistent/path.json"));
        assert!(loaded.repos.is_empty());
    }
}

/// nw-233: the sidecar must survive a multi-repo database.
#[cfg(test)]
mod repo_dimension_tests {
    use super::*;

    fn scores(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs
            .iter()
            .map(|(path, score)| ((*path).to_string(), *score))
            .collect()
    }

    /// THE BUG. `save_git_activity` was a plain write of just the incoming
    /// repo, so in a 42-repo database indexing any repo erased the other 41.
    #[test]
    fn writing_one_repo_conserves_every_other_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.gitactivity.json");

        save_git_activity_for_repo("repo:a", &scores(&[("src/main.rs", 0.9)]), &path).unwrap();
        save_git_activity_for_repo("repo:b", &scores(&[("src/main.rs", 0.1)]), &path).unwrap();

        let loaded = load_git_activity_sidecar(&path);
        assert_eq!(
            loaded.repos.len(),
            2,
            "indexing repo B must not erase repo A"
        );
        // And the collision: BOTH repos have `src/main.rs`, and each keeps its
        // OWN score. A flat map could not represent this at all.
        assert_eq!(loaded.repos["repo:a"]["src/main.rs"], 0.9);
        assert_eq!(loaded.repos["repo:b"]["src/main.rs"], 0.1);
    }

    /// Within a repo it is REPLACE, not merge: a re-index recomputes that
    /// repo's scores in full, so a path that no longer exists must not survive.
    #[test]
    fn reindexing_a_repo_replaces_its_slice_rather_than_accumulating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.gitactivity.json");

        save_git_activity_for_repo("repo:a", &scores(&[("gone.rs", 0.5)]), &path).unwrap();
        save_git_activity_for_repo("repo:a", &scores(&[("kept.rs", 0.7)]), &path).unwrap();

        let loaded = load_git_activity_sidecar(&path);
        assert_eq!(loaded.repos["repo:a"].len(), 1);
        assert!(
            !loaded.repos["repo:a"].contains_key("gone.rs"),
            "a deleted path must not linger in the repo's slice"
        );
    }

    /// A v1 flat map is DISCARDED, not migrated. Migration is unsound: a v1 key
    /// carries no repo, so attributing it to one is the exact mis-attribution
    /// the format exists to prevent. Degrading to empty is safe — a missing
    /// score is a multiplier of exactly 1.0.
    #[test]
    fn a_legacy_v1_flat_file_is_discarded_not_misattributed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.gitactivity.json");
        std::fs::write(&path, r#"{"src/main.rs":0.9,"README.md":0.4}"#).unwrap();

        let loaded = load_git_activity_sidecar(&path);

        assert!(
            loaded.repos.is_empty(),
            "a v1 flat map has no repo dimension; inventing one would be the bug"
        );
        assert_eq!(loaded.version, GITACTIVITY_VERSION);
    }

    /// A future version is discarded too, not read as if it were ours.
    #[test]
    fn a_newer_version_is_discarded_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.gitactivity.json");
        std::fs::write(
            &path,
            r#"{"version":99,"repos":{"repo:a":{"src/main.rs":0.9}}}"#,
        )
        .unwrap();

        assert!(load_git_activity_sidecar(&path).repos.is_empty());
    }

    /// The counterweight: a well-formed current-version file must round-trip,
    /// or "discard on mismatch" would quietly become "discard everything".
    #[test]
    fn a_current_version_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.gitactivity.json");

        save_git_activity_for_repo("repo:a", &scores(&[("src/main.rs", 0.9)]), &path).unwrap();

        let loaded = load_git_activity_sidecar(&path);
        assert_eq!(loaded.repos["repo:a"]["src/main.rs"], 0.9);
    }
}
