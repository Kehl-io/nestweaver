//! nw-037: the measured recall/precision loop for affected_tests-in-CI.
//!
//! The trust contract (status/gate/coverage/blind spots) is qualitative; the
//! leaders in test selection publish *measured* unsafety (Facebook PTS ships
//! over 99.9% faulty-change recall as a production requirement; Launchable derives
//! confidence curves from the customer's own history). This module closes that
//! gap with a local, dashboard-free loop:
//!
//!   1. every `affected_tests` selection is appended to
//!      `<db>.rts_selections.jsonl` (opt out: `NESTWEAVER_RTS_NO_RECORD=1`);
//!   2. CI reports full-suite outcomes via `nestweaver rts-eval record-truth`
//!      into `<db>.rts_truth.jsonl`;
//!   3. `rts-eval report` joins the two by commit sha and emits rolling
//!      file-recall / change-recall / selection-breadth / time-saved numbers —
//!      every metric carries its `n`, and below `MIN_JOINED_FOR_METRICS`
//!      joined pairs the report refuses to print percentages at all
//!      (insufficient data must never read as a measured guarantee).
//!
//! Recording is strictly non-fatal: a broken sidecar must never degrade or
//! fail the analysis it observes.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::affected_tests::AffectedTestsResult;

/// Sidecar suffix for recorded selections.
pub const SELECTIONS_SUFFIX: &str = ".rts_selections.jsonl";
/// Sidecar suffix for recorded ground truth (full-suite outcomes).
pub const TRUTH_SUFFIX: &str = ".rts_truth.jsonl";
/// Sidecar suffix for the cached latest report (feeds the in-band `measured`
/// disclosure on affected_tests results).
pub const REPORT_SUFFIX: &str = ".rts_report.json";

/// Cap on retained selection/truth records; oldest are dropped on overflow.
pub const MAX_RECORDS: usize = 10_000;

/// Below this many joined (selection, truth) pairs the report refuses to emit
/// percentages: a recall number derived from a handful of runs reads as a
/// measured guarantee while carrying none of its weight.
pub const MIN_JOINED_FOR_METRICS: usize = 10;

/// How many most-recent truth records feed the always-include-previously-
/// failing rule (TIA: previous run's failures; Develocity: "recently failed").
pub const RECENT_TRUTH_WINDOW: usize = 5;

/// Env var that disables selection recording entirely.
pub const NO_RECORD_ENV: &str = "NESTWEAVER_RTS_NO_RECORD";

/// One recorded affected_tests selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionRecord {
    /// RFC3339 timestamp of the selection.
    pub ts: String,
    /// Owning repo of the changed symbols ("" when unresolved).
    pub repo_uid: String,
    /// Commit sha the selection was computed against ("unknown" when
    /// unresolvable).
    pub sha: String,
    pub changed_files: Vec<String>,
    /// Selected test files, tiers flattened (tier membership does not affect
    /// the safety question "was the failing test selected at all").
    pub selected_test_files: Vec<String>,
    pub status: String,
    pub recommendation: String,
}

/// One recorded full-suite outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruthRecord {
    pub ts: String,
    pub repo_uid: String,
    pub sha: String,
    /// Test files that failed in the full run (empty = green run).
    pub failed_test_files: Vec<String>,
    /// Total test files executed, when the reporter knows it (feeds the
    /// time-saved proxy; None when not provided).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_test_files: Option<usize>,
    /// Failures the reporter identified as FLAKY (e.g. they passed on rerun).
    /// Excluded from recall entirely: a flaky failure is not evidence the
    /// selection missed anything, and counting it inflates recall.
    #[serde(default)]
    pub flaky_test_files: Vec<String>,
    /// How many times failures were re-run before being reported. `None` or 0
    /// means the failures are UNCONFIRMED — recall computed over them can err
    /// in either direction and is flagged uncertain, not a bound (Meta
    /// measured a ~20-point recall illusion from not de-flaking; ICSE-SEIP
    /// 2019).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reruns: Option<u32>,
}

impl TruthRecord {
    /// Failures that count as evidence: reported failures minus those the
    /// reporter flagged flaky.
    pub fn confirmed_failures(&self) -> Vec<&String> {
        self.failed_test_files
            .iter()
            .filter(|f| !self.flaky_test_files.contains(f))
            .collect()
    }

    /// Whether this record's failures were rerun-confirmed.
    pub fn is_confirmed(&self) -> bool {
        self.failed_test_files.is_empty() || self.reruns.unwrap_or(0) > 0
    }
}

/// The joined metrics. All ratio fields are `None` until
/// [`MIN_JOINED_FOR_METRICS`] pairs exist — absence IS the honesty signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtsEvalReport {
    /// Joined (selection, truth) pairs the metrics are computed over.
    pub n_joined: usize,
    /// Selections with no matching truth record (disclosed, excluded).
    pub n_unresolved_selections: usize,
    /// Truth records with no matching selection (disclosed, excluded).
    pub n_unmatched_truths: usize,
    /// True when `n_joined < MIN_JOINED_FOR_METRICS`; all ratios are None.
    pub insufficient_data: bool,
    /// Of all failed test files across joined pairs, the fraction that the
    /// selection had included (the safety-relevant number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_recall: Option<f64>,
    /// Of joined pairs with >=1 failure, the fraction where >=1 failed file
    /// was selected (Facebook's faulty-change-recall analogue).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_recall: Option<f64>,
    /// Mean fraction of the full suite the selection asked to run, over
    /// joined pairs whose truth carried total_test_files. Labelled breadth,
    /// not precision: unexecuted selected tests have unknown outcomes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_breadth: Option<f64>,
    /// 1 - breadth, over the same pairs: the file-count proxy for CI time
    /// saved (NOT wall-clock).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_saved_proxy: Option<f64>,
    /// Failing joined pairs, for context on how much signal recall rests on.
    pub n_failing_pairs: usize,
    /// Window the report was computed over (last N joined pairs; 0 = all).
    pub window: usize,
    /// Joined pairs whose failures were NOT rerun-confirmed. While this is
    /// non-zero the recall figures are UNCERTAIN in both directions:
    /// unconfirmed failures excluded as flaky shrink the denominator (recall
    /// drifts up), but unconfirmed failures the selection caught are also
    /// excluded from the numerator (recall drifts down) — so no bound
    /// direction can be claimed.
    pub unconfirmed_failure_runs: usize,
    /// Failures excluded as flaky across the window.
    pub excluded_flaky_failures: usize,
    /// Set when `unconfirmed_failure_runs > 0`: the recall estimate rests on
    /// partially unconfirmed evidence and can err in either direction — it is
    /// NOT necessarily an upper bound (P2 review).
    pub recall_estimate_uncertain: bool,
}

/// In-band measured-recall disclosure attached to affected_tests results
/// once enough joined pairs exist. A compressed view of [`RtsEvalReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasuredRecall {
    pub file_recall: f64,
    pub change_recall: f64,
    pub n_joined: usize,
    pub window: usize,
    /// True when part of the underlying evidence was never rerun-confirmed —
    /// the recall figures can err in EITHER direction (they are not a
    /// bound). Consumers must not present the numbers as exact without this
    /// caveat (P1 review).
    pub recall_estimate_uncertain: bool,
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

fn recording_disabled() -> bool {
    std::env::var(NO_RECORD_ENV).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Advisory lock guarding read-modify-write cycles on the rts-eval sidecars.
/// `append_jsonl` (append + rotation) and `record_truth` (read + upsert +
/// atomic replace) are both read-modify-write: two concurrent recorders
/// (parallel CI steps sharing a DB) can silently lose each other's records
/// without mutual exclusion. Pattern mirrors investigate.rs's
/// `BundleStoreLock`: `create_new` lock file, bounded wait, stale-lock
/// break, RAII cleanup — plus an ownership TOKEN written into the lock file:
/// after a stale-lock takeover, the previous holder's Drop must not delete
/// the successor's lock (that would void mutual exclusion for the next
/// contender, P2 review). Acquisition failure is an ERROR, not a silent
/// unlocked pass — proceeding unlocked would reintroduce the very
/// lost-update race the lock exists to close (P2 review). Recording
/// failures surface as a non-fatal Note notification on affected-tests,
/// so erroring here never breaks the selection itself.
struct SidecarLock {
    path: PathBuf,
    token: String,
}

impl SidecarLock {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    const STALE: std::time::Duration = std::time::Duration::from_secs(60);

    fn acquire(sidecar: &Path) -> Result<Self> {
        let path = sidecar.with_extension("lock");
        let token = format!(
            "{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let start = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(mut f) => {
                    // Best-effort: the token lets a stale-taken-over holder
                    // recognize the lock is no longer its own on Drop.
                    let _ = f.write_all(token.as_bytes());
                    drop(f);
                    // Create-then-verify: two contenders can both observe a
                    // stale lock, and the loser's remove+create can land
                    // AFTER the winner's create. Verify the file still holds
                    // OUR token before claiming the lock; if a contender
                    // replaced it, treat the acquisition as contention and
                    // wait for the real holder instead of co-owning (P1).
                    let ours = std::fs::read_to_string(&path)
                        .map(|content| content == token)
                        .unwrap_or(false);
                    if ours {
                        return Ok(Self { path, token });
                    }
                    if start.elapsed() >= Self::WAIT {
                        anyhow::bail!(
                            "timed out acquiring rts-eval sidecar lock {} after {:?} \
                             (another recorder is holding it)",
                            path.display(),
                            Self::WAIT
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Break a lock abandoned by a crashed/killed holder.
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Self::STALE);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if start.elapsed() >= Self::WAIT {
                        anyhow::bail!(
                            "timed out acquiring rts-eval sidecar lock {} after {:?} \
                             (another recorder is holding it)",
                            path.display(),
                            Self::WAIT
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("create rts-eval sidecar lock {}", path.display())
                    });
                }
            }
        }
    }
}

impl Drop for SidecarLock {
    fn drop(&mut self) {
        // Only remove the lock if it is still OURS — a stale-lock takeover
        // may have replaced it with a successor's lock, which must survive.
        let still_ours = std::fs::read_to_string(&self.path)
            .map(|content| content == self.token)
            .unwrap_or(false);
        if still_ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Append one JSON line to `path`, enforcing [`MAX_RECORDS`] by rewriting
/// with the oldest lines dropped when the cap is exceeded.
fn append_jsonl(path: &Path, line: &str) -> Result<()> {
    // Hold the sidecar lock across append + rotation check: two concurrent
    // recorders interleaving here can each rotate-rewrite over the other's
    // fresh append and silently lose records (P2 review).
    let _lock = SidecarLock::acquire(path)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open {} for append", path.display()))?;
        f.write_all(line.as_bytes()).context("append record")?;
        f.write_all(b"\n").context("append newline")?;
    }
    // Rotation: cheap line count; rewrite only on overflow. The rewrite
    // goes through atomic_replace_file (tmp + fsync + rename) — a bare
    // `std::fs::write` truncates in place, so a crash mid-rotation would
    // destroy the whole history (P2 review).
    let content = std::fs::read_to_string(path).context("read for rotation check")?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > MAX_RECORDS {
        let keep = &lines[lines.len() - MAX_RECORDS..];
        let mut out = keep.join("\n");
        out.push('\n');
        nestweaver_store::durable_sidecar::atomic_replace_file(path, |f| {
            f.write_all(out.as_bytes())
        })
        .with_context(|| format!("rewrite rotated sidecar {}", path.display()))?;
    }
    Ok(())
}

/// Load all parseable records from a JSONL sidecar. Corrupt lines are skipped
/// (their count is returned) — one bad line must not disable measurement.
fn load_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> (Vec<T>, usize) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let mut out = Vec::new();
    let mut corrupt = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(r) => out.push(r),
            Err(_) => corrupt += 1,
        }
    }
    (out, corrupt)
}

/// Record an affected_tests selection. Never fails the caller: all errors are
/// returned for the caller to surface as a Note-level notification.
pub fn record_selection(
    db_path: &Path,
    result: &AffectedTestsResult,
    repo_uid: &str,
    sha: &str,
) -> Result<()> {
    if recording_disabled() {
        return Ok(());
    }
    let selected: Vec<String> = result
        .tier_1
        .iter()
        .chain(&result.tier_2)
        .chain(&result.tier_3)
        .map(|f| f.test_file.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let rec = SelectionRecord {
        ts: now_rfc3339(),
        repo_uid: repo_uid.to_string(),
        sha: sha.to_string(),
        changed_files: result.changed_files.clone(),
        selected_test_files: selected,
        status: format!("{:?}", result.status).to_lowercase(),
        recommendation: result.recommendation.clone(),
    };
    let line = serde_json::to_string(&rec).context("serialize selection record")?;
    append_jsonl(&crate::sidecar_path(db_path, SELECTIONS_SUFFIX), &line)
}

/// Record a full-suite outcome (ground truth) for `sha`.
///
/// A re-record for the same `(repo_uid, sha)` UPSERTS: the report join takes
/// the first matching truth, so appending a corrected record after the
/// original would silently keep the stale one forever (F-low). Replacements
/// are logged, not silent.
#[allow(clippy::too_many_arguments)]
pub fn record_truth(
    db_path: &Path,
    repo_uid: &str,
    sha: &str,
    failed_test_files: &[String],
    total_test_files: Option<usize>,
    flaky_test_files: &[String],
    reruns: Option<u32>,
) -> Result<()> {
    let rec = TruthRecord {
        ts: now_rfc3339(),
        repo_uid: repo_uid.to_string(),
        sha: sha.to_string(),
        failed_test_files: failed_test_files.to_vec(),
        total_test_files,
        flaky_test_files: flaky_test_files.to_vec(),
        reruns,
    };
    let line = serde_json::to_string(&rec).context("serialize truth record")?;
    let path = crate::sidecar_path(db_path, TRUTH_SUFFIX);
    // Hold the sidecar lock across the whole read → upsert → atomic replace
    // cycle so concurrent recorders don't lose each other's updates.
    let _lock = SidecarLock::acquire(&path)?;

    // Drop any existing record for the same (repo_uid, sha) before appending.
    // Unparseable lines are preserved verbatim (one bad line must not disable
    // measurement), matching load_jsonl's skip-corrupt tolerance.
    let mut lines: Vec<String> = match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let before = lines.len();
    lines.retain(|l| {
        serde_json::from_str::<TruthRecord>(l)
            .map(|t| !(t.sha == rec.sha && t.repo_uid == rec.repo_uid))
            .unwrap_or(true)
    });
    let replaced = before - lines.len();
    if replaced > 0 {
        tracing::warn!(
            sha = %rec.sha,
            repo_uid = %rec.repo_uid,
            replaced,
            "rts-eval record-truth: replacing existing truth record(s) for this sha"
        );
    }
    lines.push(line);
    // Rotation: oldest records dropped on overflow, same cap as append_jsonl.
    if lines.len() > MAX_RECORDS {
        lines.drain(..lines.len() - MAX_RECORDS);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    // Atomic replace (write tmp + rename + sync) instead of a bare rewrite:
    // `std::fs::write` truncates in place, so a crash mid-write would destroy
    // the entire truth history. Mirrors the generation sidecar's durability.
    nestweaver_store::durable_sidecar::atomic_replace_file(&path, |f| f.write_all(out.as_bytes()))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Join selections with truth by (sha, repo_uid when both carry one) and
/// compute the report over the last `window` joined pairs (0 = all). The
/// cached report is written to `<db>.rts_report.json` for the in-band
/// `measured` disclosure.
pub fn compute_report(db_path: &Path, window: usize) -> Result<RtsEvalReport> {
    let (selections, _sel_corrupt) =
        load_jsonl::<SelectionRecord>(&crate::sidecar_path(db_path, SELECTIONS_SUFFIX));
    let (truths, _truth_corrupt) =
        load_jsonl::<TruthRecord>(&crate::sidecar_path(db_path, TRUTH_SUFFIX));

    // Join 1:1, truth-centric and temporal. Each truth pairs with the
    // LATEST still-unmatched selection at the same sha recorded at or
    // before the truth's own timestamp — that is the run the truth
    // actually measures; pairing it with an arbitrary earlier selection
    // at the same sha could score the outcome against the wrong selected
    // set (P1 review), and pairing it with a FUTURE selection (recorded
    // after the truth) would invert causality, so the fallback for
    // timestamp-less records is restricted to selections that also carry
    // no timestamp (P1 review). Each truth is consumed at most once, so N
    // selector re-runs against one tested commit can't manufacture sample
    // size (P1 review).
    let mut joined: Vec<(&SelectionRecord, &TruthRecord)> = Vec::new();
    let mut matched_sel: HashSet<usize> = HashSet::new();
    let mut matched_truth: HashSet<usize> = HashSet::new();
    let sel_matches = |s: &&SelectionRecord, t: &TruthRecord| {
        s.sha == t.sha
            && (t.repo_uid.is_empty() || s.repo_uid.is_empty() || t.repo_uid == s.repo_uid)
    };
    for (t_idx, truth) in truths.iter().enumerate() {
        if truth.sha == "unknown" || truth.sha.is_empty() {
            continue;
        }
        // Eligible selection with the GREATEST timestamp at or before the
        // truth's own (RFC3339 compares lexicographically) — not simply the
        // last in append order, since parallel recorders can append out of
        // timestamp order (P2 review). When no selection predates the truth,
        // only a timestamp-less selection may pair — never a future-dated one.
        let hit = selections
            .iter()
            .enumerate()
            .filter(|(i, s)| !matched_sel.contains(i) && sel_matches(s, truth))
            .filter(|(_, s)| !s.ts.is_empty() && s.ts <= truth.ts)
            .max_by(|(_, a), (_, b)| a.ts.cmp(&b.ts))
            .or_else(|| {
                selections.iter().enumerate().find(|(i, s)| {
                    !matched_sel.contains(i) && sel_matches(s, truth) && s.ts.is_empty()
                })
            });
        if let Some((s_idx, sel)) = hit {
            matched_sel.insert(s_idx);
            matched_truth.insert(t_idx);
            joined.push((sel, truth));
        }
    }
    let unresolved = selections.len() - matched_sel.len();
    let n_unmatched_truths = truths.len() - matched_truth.len();

    if window > 0 && joined.len() > window {
        let start = joined.len() - window;
        joined.drain(..start);
    }

    let n_joined = joined.len();
    let insufficient = n_joined < MIN_JOINED_FOR_METRICS;

    let mut failed_total = 0usize;
    let mut failed_caught = 0usize;
    let mut failing_pairs = 0usize;
    let mut failing_pairs_caught = 0usize;
    let mut breadth_sum = 0.0f64;
    let mut breadth_n = 0usize;
    let mut unconfirmed_failure_runs = 0usize;
    let mut excluded_flaky_failures = 0usize;
    for (sel, truth) in &joined {
        let selected: HashSet<&str> = sel.selected_test_files.iter().map(|s| s.as_str()).collect();
        // nw-066: only rerun-CONFIRMED, non-flaky failures are evidence. A
        // flaky failure the selection didn't pick is not a miss, and counting
        // it distorts recall (Meta measured a ~20-point illusion from
        // computing recall over non-de-flaked outcomes).
        let confirmed = truth.confirmed_failures();
        excluded_flaky_failures += truth.failed_test_files.len() - confirmed.len();
        if !truth.failed_test_files.is_empty() && !truth.is_confirmed() {
            unconfirmed_failure_runs += 1;
        }
        if !confirmed.is_empty() {
            failing_pairs += 1;
            let caught_any = confirmed.iter().any(|f| selected.contains(f.as_str()));
            if caught_any {
                failing_pairs_caught += 1;
            }
            for f in &confirmed {
                failed_total += 1;
                if selected.contains(f.as_str()) {
                    failed_caught += 1;
                }
            }
        }
        if let Some(total) = truth.total_test_files
            && total > 0
        {
            breadth_sum += sel.selected_test_files.len() as f64 / total as f64;
            breadth_n += 1;
        }
    }

    let ratio = |num: usize, den: usize| -> Option<f64> {
        if insufficient || den == 0 {
            None
        } else {
            Some(num as f64 / den as f64)
        }
    };
    let breadth = if insufficient || breadth_n == 0 {
        None
    } else {
        Some(breadth_sum / breadth_n as f64)
    };

    let report = RtsEvalReport {
        n_joined,
        n_unresolved_selections: unresolved,
        n_unmatched_truths,
        insufficient_data: insufficient,
        file_recall: ratio(failed_caught, failed_total),
        change_recall: ratio(failing_pairs_caught, failing_pairs),
        selection_breadth: breadth,
        time_saved_proxy: breadth.map(|b| 1.0 - b),
        n_failing_pairs: failing_pairs,
        window,
        unconfirmed_failure_runs,
        excluded_flaky_failures,
        recall_estimate_uncertain: unconfirmed_failure_runs > 0,
    };

    // Cache for the in-band `measured` disclosure; best-effort. Refresh it
    // from ANY report that clears the honesty bar — the disclosure always
    // carries its own n_joined/window, so a sufficient windowed report is a
    // valid measured claim. An INSUFFICIENT report is never cached, so a
    // thin slice can't strip the disclosure from subsequent affected-tests
    // runs, and the default 50-window workflow keeps refreshing as history
    // grows past 50 joined pairs (P1 review).
    if !report.insufficient_data
        && let Ok(json) = serde_json::to_string_pretty(&report)
    {
        let _ = std::fs::write(crate::sidecar_path(db_path, REPORT_SUFFIX), json);
    }

    Ok(report)
}

/// Run `affected_tests` with the nw-037 measurement loop attached: record
/// the selection (non-fatal; failure surfaces as a Note notification) and
/// attach the in-band `measured` disclosure when the data clears the bar.
/// `db_path: None` (no sidecar home) degrades to the plain analysis.
pub fn run_recorded(
    store: &nestweaver_store::GraphStore,
    changed_files: &[String],
    db_path: Option<&Path>,
) -> Result<AffectedTestsResult> {
    let mut result = crate::affected_tests::affected_tests(store, changed_files)?;
    let Some(db) = db_path else {
        return Ok(result);
    };

    // Owning repo of the selection = the first changed symbol's repo; the
    // sha it was computed against = that repo's working-tree HEAD when a
    // local root is known, else its indexed sha, else "unknown".
    let repo_uid = result
        .changed_symbols
        .first()
        .map(|cs| cs.repo_uid.clone())
        .unwrap_or_default();
    let sha = resolve_selection_sha(store, &repo_uid);

    // nw-064 always-include: test files that failed in the most recent
    // full-suite runs are selected regardless of the static graph (TIA
    // includes the previous run's failures; Develocity always selects
    // recently-failed tests). Runs BEFORE record_selection so the recorded
    // selection — and therefore the measured recall — reflects the widened
    // set the CI job is actually told to run.
    {
        let (truths, _) = load_jsonl::<TruthRecord>(&crate::sidecar_path(db, TRUTH_SUFFIX));
        let recent = truths
            .iter()
            .rev()
            .filter(|t| t.repo_uid.is_empty() || repo_uid.is_empty() || t.repo_uid == repo_uid)
            .take(RECENT_TRUTH_WINDOW);
        let selected: HashSet<String> = result
            .tier_1
            .iter()
            .chain(&result.tier_2)
            .chain(&result.tier_3)
            .map(|f| f.test_file.clone())
            .collect();
        let mut included: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for t in recent {
            // nw-066: never pin a FLAKY failure into every future selection —
            // that silently widens selections forever off a non-signal.
            for f in t.confirmed_failures() {
                if !selected.contains(f) && seen.insert(f.clone()) {
                    included.push(f.clone());
                }
            }
        }
        if !included.is_empty() {
            for f in &included {
                result.tier_1.push(crate::affected_tests::AffectedTestFile {
                    test_file: f.clone(),
                    tests: Vec::new(),
                    symbol_uid: String::new(),
                    confidence: 1.0,
                });
            }
            result
                .notifications
                .push(crate::blast_radius::Notification {
                    level: crate::blast_radius::NotificationLevel::Note,
                    message: format!(
                        "always-included {} recently-failed test file(s) from the last {} full-suite run(s): {}",
                        included.len(),
                        RECENT_TRUTH_WINDOW,
                        included.join(", ")
                    ),
                    descriptor: "always-include-previously-failing".to_string(),
                });
        }
    }

    if let Err(e) = record_selection(db, &result, &repo_uid, &sha) {
        result
            .notifications
            .push(crate::blast_radius::Notification {
                level: crate::blast_radius::NotificationLevel::Note,
                message: format!("recording this selection for recall measurement failed: {e}"),
                descriptor: "rts-record-failed".to_string(),
            });
    }
    result.measured = load_measured(db);
    Ok(result)
}

/// Best-effort sha resolution for a selection record.
fn resolve_selection_sha(store: &nestweaver_store::GraphStore, repo_uid: &str) -> String {
    if repo_uid.is_empty() {
        return "unknown".to_string();
    }
    let Ok(repos) = store.list_repos(None) else {
        return "unknown".to_string();
    };
    let Some(repo) = repos.iter().find(|r| r.uid == repo_uid) else {
        return "unknown".to_string();
    };
    if let Some(root) = repo.local_root()
        && let Ok(out) = std::process::Command::new("git")
            .args(["-C", root, "rev-parse", "HEAD"])
            .output()
        && out.status.success()
    {
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !head.is_empty() {
            return head;
        }
    }
    if !repo.indexed_sha.is_empty() {
        return repo.indexed_sha.clone();
    }
    "unknown".to_string()
}

/// Load the cached report and compress it to the in-band disclosure, when the
/// data clears the honesty bar. Absence is meaningful: no `measured` field
/// means "no measured claim".
pub fn load_measured(db_path: &Path) -> Option<MeasuredRecall> {
    let content = std::fs::read_to_string(crate::sidecar_path(db_path, REPORT_SUFFIX)).ok()?;
    let report: RtsEvalReport = serde_json::from_str(&content).ok()?;
    if report.insufficient_data {
        return None;
    }
    Some(MeasuredRecall {
        file_recall: report.file_recall?,
        change_recall: report.change_recall?,
        n_joined: report.n_joined,
        window: report.window,
        recall_estimate_uncertain: report.recall_estimate_uncertain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affected_tests::{AffectedTestFile, AffectedTestsResult};
    use crate::blast_radius::AnalysisStatus;

    fn scratch_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("scratch.lbug");
        std::fs::write(&db, b"").expect("touch db");
        (dir, db)
    }

    fn result_selecting(files: &[&str]) -> AffectedTestsResult {
        AffectedTestsResult {
            changed_files: vec!["src/a.rs".to_string()],
            changed_symbols: Vec::new(),
            tier_1: files
                .iter()
                .map(|f| AffectedTestFile {
                    test_file: f.to_string(),
                    tests: vec!["t".to_string()],
                    symbol_uid: "sym:t".to_string(),
                    confidence: 0.9,
                })
                .collect(),
            tier_2: Vec::new(),
            tier_3: Vec::new(),
            summary: String::new(),
            disclaimer: String::new(),
            status: AnalysisStatus::Complete,
            notifications: Vec::new(),
            recommendation: "selection-usable".to_string(),
            measured: None,
        }
    }

    #[test]
    fn record_selection_appends_jsonl() {
        let (_dir, db) = scratch_db();
        let r = result_selecting(&["tests/a.test.ts"]);
        record_selection(&db, &r, "repo:1", "abc123").expect("record");
        record_selection(&db, &r, "repo:1", "def456").expect("record 2");
        let (loaded, corrupt) =
            load_jsonl::<SelectionRecord>(&crate::sidecar_path(&db, SELECTIONS_SUFFIX));
        assert_eq!(loaded.len(), 2);
        assert_eq!(corrupt, 0);
        assert_eq!(loaded[0].sha, "abc123");
        assert_eq!(loaded[0].selected_test_files, vec!["tests/a.test.ts"]);
        assert_eq!(loaded[0].recommendation, "selection-usable");
    }

    #[test]
    fn load_skips_corrupt_lines() {
        let (_dir, db) = scratch_db();
        let r = result_selecting(&["tests/a.test.ts"]);
        record_selection(&db, &r, "repo:1", "abc123").expect("record");
        let path = crate::sidecar_path(&db, SELECTIONS_SUFFIX);
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("{not json at all\n");
        std::fs::write(&path, content).unwrap();
        record_selection(&db, &r, "repo:1", "def456").expect("record after corrupt");
        let (loaded, corrupt) = load_jsonl::<SelectionRecord>(&path);
        assert_eq!(loaded.len(), 2, "valid records survive a corrupt line");
        assert_eq!(corrupt, 1);
    }

    #[test]
    fn record_truth_upserts_duplicate_sha() {
        // F-low: re-recording truth for the same (repo, sha) must REPLACE the
        // stale record — the report join takes the first match, so appending
        // would silently keep the original forever.
        let (_dir, db) = scratch_db();
        record_truth(
            &db,
            "repo:1",
            "abc123",
            &["tests/a.test.ts".to_string()],
            Some(10),
            &[],
            None,
        )
        .expect("first");
        record_truth(&db, "repo:1", "abc123", &[], Some(10), &[], Some(2)).expect("correction");
        // A different sha and a different repo are untouched.
        record_truth(&db, "repo:1", "def456", &[], Some(10), &[], Some(2)).expect("other sha");
        record_truth(
            &db,
            "repo:2",
            "abc123",
            &["tests/z.test.ts".to_string()],
            None,
            &[],
            None,
        )
        .expect("other repo");

        let (loaded, corrupt) = load_jsonl::<TruthRecord>(&crate::sidecar_path(&db, TRUTH_SUFFIX));
        assert_eq!(corrupt, 0);
        let matching: Vec<_> = loaded
            .iter()
            .filter(|t| t.sha == "abc123" && t.repo_uid == "repo:1")
            .collect();
        assert_eq!(matching.len(), 1, "duplicate sha must upsert, not append");
        assert!(
            matching[0].failed_test_files.is_empty(),
            "the corrected record must win"
        );
        assert_eq!(matching[0].reruns, Some(2));
        assert_eq!(loaded.len(), 3);
    }

    #[test]
    fn record_truth_writes_atomically_without_leftover_temp_files() {
        // record_truth rewrites the whole JSONL sidecar; a bare
        // `std::fs::write` would truncate in place and a crash mid-write
        // would destroy the entire truth history. The write goes through
        // `durable_sidecar::atomic_replace_file` (temp + rename), so after
        // the call the directory holds only the final sidecar — no temp
        // debris — and the content round-trips.
        let (dir, db) = scratch_db();
        record_truth(
            &db,
            "repo:1",
            "abc123",
            &["tests/a.test.ts".to_string()],
            Some(10),
            &[],
            None,
        )
        .expect("first");
        record_truth(&db, "repo:1", "abc123", &[], Some(10), &[], Some(1)).expect("upsert");

        let (loaded, corrupt) = load_jsonl::<TruthRecord>(&crate::sidecar_path(&db, TRUTH_SUFFIX));
        assert_eq!(corrupt, 0);
        assert_eq!(loaded.len(), 1, "upserted content must round-trip");
        assert_eq!(loaded[0].reruns, Some(1));

        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "scratch.lbug" && !name.ends_with(TRUTH_SUFFIX))
            .collect();
        assert!(
            stray.is_empty(),
            "atomic replace must not leave temp debris: {stray:?}"
        );
    }

    #[test]
    fn report_round_trip_hand_checkable() {
        let (_dir, db) = scratch_db();
        // 12 selections/truths at distinct shas so we clear the n>=10 bar.
        // Selection always picks tests/a.test.ts; truth: two runs fail —
        // one where a.test.ts failed (caught), one where b.test.ts failed
        // (missed). Total suite = 10 files each run.
        for i in 0..12 {
            let sha = format!("sha{i}");
            record_selection(&db, &result_selecting(&["tests/a.test.ts"]), "repo:1", &sha)
                .expect("sel");
            let failed: Vec<String> = match i {
                3 => vec!["tests/a.test.ts".to_string()],
                7 => vec!["tests/b.test.ts".to_string()],
                _ => Vec::new(),
            };
            record_truth(&db, "repo:1", &sha, &failed, Some(10), &[], Some(3)).expect("truth");
        }
        let report = compute_report(&db, 0).expect("report");
        assert_eq!(report.n_joined, 12);
        assert!(!report.insufficient_data);
        assert_eq!(report.n_failing_pairs, 2);
        // 2 failed files total, 1 caught.
        assert_eq!(report.file_recall, Some(0.5));
        // 2 failing runs, 1 with >=1 caught.
        assert_eq!(report.change_recall, Some(0.5));
        // 1 selected of 10 total, every run.
        assert!((report.selection_breadth.unwrap() - 0.1).abs() < 1e-9);
        assert!((report.time_saved_proxy.unwrap() - 0.9).abs() < 1e-9);
        // Cached report feeds the in-band disclosure.
        let measured = load_measured(&db).expect("measured present at n>=10");
        assert_eq!(measured.n_joined, 12);
        assert_eq!(measured.file_recall, 0.5);
    }

    /// nw-066: a FLAKY failure must not count as a miss (it would deflate
    /// recall), must not pin itself into future selections, and an
    /// UNCONFIRMED failure run must mark the recall estimate as uncertain.
    #[test]
    fn flaky_failures_excluded_and_unconfirmed_flagged() {
        let (_dir, db) = scratch_db();
        // 12 pairs: selection always picks a.test.ts. Run 3 fails b.test.ts
        // but reports it FLAKY -> excluded entirely (recall stays perfect).
        for i in 0..12 {
            let sha = format!("sha{i}");
            record_selection(&db, &result_selecting(&["tests/a.test.ts"]), "repo:1", &sha)
                .expect("sel");
            let (failed, flaky): (Vec<String>, Vec<String>) = if i == 3 {
                (
                    vec!["tests/b.test.ts".to_string()],
                    vec!["tests/b.test.ts".to_string()],
                )
            } else if i == 5 {
                (vec!["tests/a.test.ts".to_string()], vec![])
            } else {
                (vec![], vec![])
            };
            record_truth(&db, "repo:1", &sha, &failed, Some(10), &flaky, Some(3)).expect("truth");
        }
        let r = compute_report(&db, 0).expect("report");
        assert_eq!(r.excluded_flaky_failures, 1, "flaky failure excluded");
        // Only the confirmed a.test.ts failure counts, and it WAS selected.
        assert_eq!(r.n_failing_pairs, 1);
        assert_eq!(
            r.file_recall,
            Some(1.0),
            "flaky miss must not deflate recall"
        );
        assert!(
            !r.recall_estimate_uncertain,
            "all runs were rerun-confirmed"
        );
        assert_eq!(r.unconfirmed_failure_runs, 0);
    }

    #[test]
    fn unconfirmed_failures_mark_recall_estimate_uncertain() {
        let (_dir, db) = scratch_db();
        for i in 0..12 {
            let sha = format!("sha{i}");
            record_selection(&db, &result_selecting(&["tests/a.test.ts"]), "repo:1", &sha)
                .expect("sel");
            let failed = if i == 4 {
                vec!["tests/a.test.ts".to_string()]
            } else {
                vec![]
            };
            // reruns: None => failures were never re-run => unconfirmed.
            record_truth(&db, "repo:1", &sha, &failed, Some(10), &[], None).expect("truth");
        }
        let r = compute_report(&db, 0).expect("report");
        assert_eq!(r.unconfirmed_failure_runs, 1);
        assert!(
            r.recall_estimate_uncertain,
            "unconfirmed failures must flag the recall estimate as uncertain"
        );
    }

    /// A flaky failure must NOT be pinned into every later selection by the
    /// nw-064 always-include-previously-failing rule.
    #[test]
    fn flaky_failures_are_not_always_included() {
        use nestweaver_store::GraphStore;
        let store = GraphStore::in_memory().expect("store");
        let (_dir, db) = scratch_db();
        record_truth(
            &db,
            "",
            "sha-prev",
            &["tests/flaky.test.ts".to_string()],
            Some(10),
            &["tests/flaky.test.ts".to_string()],
            Some(3),
        )
        .expect("truth");
        let result =
            run_recorded(&store, &["src/a.rs".to_string()], Some(&db)).expect("run_recorded");
        let t1: Vec<&str> = result.tier_1.iter().map(|f| f.test_file.as_str()).collect();
        assert!(
            !t1.contains(&"tests/flaky.test.ts"),
            "flaky failure must not be pinned into selections: {t1:?}"
        );
    }

    #[test]
    fn report_below_n10_refuses_percentages() {
        let (_dir, db) = scratch_db();
        for i in 0..3 {
            let sha = format!("sha{i}");
            record_selection(&db, &result_selecting(&["tests/a.test.ts"]), "repo:1", &sha)
                .expect("sel");
            record_truth(
                &db,
                "repo:1",
                &sha,
                &["tests/a.test.ts".to_string()],
                Some(10),
                &[],
                Some(3),
            )
            .expect("truth");
        }
        let report = compute_report(&db, 0).expect("report");
        assert_eq!(report.n_joined, 3);
        assert!(report.insufficient_data);
        assert_eq!(report.file_recall, None, "no percentages below n=10");
        assert_eq!(report.change_recall, None);
        assert_eq!(report.selection_breadth, None);
        assert!(
            load_measured(&db).is_none(),
            "no in-band measured claim below the honesty bar"
        );
    }

    #[test]
    fn unresolved_selections_are_disclosed_not_counted() {
        let (_dir, db) = scratch_db();
        record_selection(
            &db,
            &result_selecting(&["tests/a.test.ts"]),
            "repo:1",
            "unknown",
        )
        .expect("sel");
        record_selection(
            &db,
            &result_selecting(&["tests/a.test.ts"]),
            "repo:1",
            "sha-without-truth",
        )
        .expect("sel2");
        let report = compute_report(&db, 0).expect("report");
        assert_eq!(report.n_joined, 0);
        assert_eq!(report.n_unresolved_selections, 2);
    }

    #[test]
    fn rotation_caps_records() {
        let (_dir, db) = scratch_db();
        let path = crate::sidecar_path(&db, SELECTIONS_SUFFIX);
        // Seed just over the cap cheaply (raw lines, same shape).
        let mut bulk = String::new();
        for i in 0..MAX_RECORDS {
            bulk.push_str(&format!(
                "{{\"ts\":\"t\",\"repo_uid\":\"r\",\"sha\":\"s{i}\",\"changed_files\":[],\"selected_test_files\":[],\"status\":\"complete\",\"recommendation\":\"selection-usable\"}}\n"
            ));
        }
        std::fs::write(&path, bulk).unwrap();
        record_selection(&db, &result_selecting(&[]), "repo:1", "newest").expect("record");
        let (loaded, _) = load_jsonl::<SelectionRecord>(&path);
        assert_eq!(loaded.len(), MAX_RECORDS, "capped at MAX_RECORDS");
        assert_eq!(
            loaded.last().map(|r| r.sha.as_str()),
            Some("newest"),
            "newest record survives rotation"
        );
        assert_ne!(
            loaded.first().map(|r| r.sha.as_str()),
            Some("s0"),
            "oldest record dropped"
        );
    }

    /// nw-064: TIA includes tests that failed in the previous run; Develocity
    /// always selects "recently failed" tests. Recently-failed test files from
    /// the truth sidecar must be tier-1 even when the static graph misses them
    /// — and the recorded selection must count them as selected.
    #[test]
    fn recently_failed_tests_are_always_included() {
        use nestweaver_store::GraphStore;
        let store = GraphStore::in_memory().expect("store");
        let (_dir, db) = scratch_db();
        record_truth(
            &db,
            "",
            "sha-prev",
            &["tests/flaky.test.ts".to_string()],
            Some(10),
            &[],
            Some(3),
        )
        .expect("truth");
        let result =
            run_recorded(&store, &["src/a.rs".to_string()], Some(&db)).expect("run_recorded");
        let t1: Vec<&str> = result.tier_1.iter().map(|f| f.test_file.as_str()).collect();
        assert!(
            t1.contains(&"tests/flaky.test.ts"),
            "recently-failed test must be always-included: {t1:?}"
        );
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "always-include-previously-failing"),
            "inclusion must be disclosed: {:?}",
            result.notifications
        );
        // The recorded selection must reflect the widened set.
        let (recs, _) = load_jsonl::<SelectionRecord>(&crate::sidecar_path(&db, SELECTIONS_SUFFIX));
        assert!(
            recs[0]
                .selected_test_files
                .contains(&"tests/flaky.test.ts".to_string()),
            "recorded selection must include the always-included file: {:?}",
            recs[0].selected_test_files
        );
    }

    #[test]
    fn run_recorded_writes_selection_and_notes_failures_nonfatally() {
        use nestweaver_store::GraphStore;
        let store = GraphStore::in_memory().expect("store");
        let (_dir, db) = scratch_db();
        let result =
            run_recorded(&store, &["src/a.rs".to_string()], Some(&db)).expect("run_recorded");
        // Empty store: selection still recorded (repo/sha unknown), analysis
        // result untouched, no measured claim.
        assert!(result.measured.is_none());
        let (loaded, _) =
            load_jsonl::<SelectionRecord>(&crate::sidecar_path(&db, SELECTIONS_SUFFIX));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].sha, "unknown");
        // Unwritable sidecar home: analysis still succeeds, with a Note.
        let ro = _dir.path().join("missing-dir").join("db.lbug");
        let degraded =
            run_recorded(&store, &["src/a.rs".to_string()], Some(&ro)).expect("nonfatal");
        assert!(
            degraded
                .notifications
                .iter()
                .any(|n| n.descriptor == "rts-record-failed"),
            "recording failure must surface as a note: {:?}",
            degraded.notifications
        );
    }

    /// P1 review: a truth recorded BEFORE the only selection at its sha must
    /// NOT pair with that future selection — the selection couldn't have
    /// produced the outcome. Timestamp-less records may still pair (legacy /
    /// hand-written sidecars).
    #[test]
    fn truth_is_never_paired_with_a_future_selection() {
        let (_dir, db) = scratch_db();
        let sel_path = crate::sidecar_path(&db, SELECTIONS_SUFFIX);
        let truth_path = crate::sidecar_path(&db, TRUTH_SUFFIX);
        let sel = serde_json::json!({
            "ts": "2026-01-02T00:00:00Z", "repo_uid": "", "sha": "sha1",
            "changed_files": [], "selected_test_files": ["tests/t.rs"],
            "status": "complete", "recommendation": "selection-usable"
        });
        std::fs::write(&sel_path, format!("{sel}\n")).expect("write selection");
        let truth_early = serde_json::json!({
            "ts": "2026-01-01T00:00:00Z", "repo_uid": "", "sha": "sha1",
            "failed_test_files": ["tests/t.rs"], "total_test_files": 10,
            "flaky_test_files": [], "reruns": 3
        });
        std::fs::write(&truth_path, format!("{truth_early}\n")).expect("write truth");

        let r = compute_report(&db, 0).expect("report");
        assert_eq!(r.n_joined, 0, "future selection must not pair");
        assert_eq!(r.n_unmatched_truths, 1);

        // Sanity: a truth recorded AFTER the selection pairs normally.
        let truth_late = serde_json::json!({
            "ts": "2026-01-03T00:00:00Z", "repo_uid": "", "sha": "sha1",
            "failed_test_files": ["tests/t.rs"], "total_test_files": 10,
            "flaky_test_files": [], "reruns": 3
        });
        std::fs::write(&truth_path, format!("{truth_late}\n")).expect("rewrite truth");
        let r2 = compute_report(&db, 0).expect("report2");
        assert_eq!(r2.n_joined, 1, "truth recorded after the selection pairs");
    }

    /// P1 review: the report cache (which feeds the in-band `measured`
    /// disclosure) must refresh for any report that covers ALL joined data —
    /// including the CLI default window of 50 — but NOT for a genuine slice.
    #[test]
    fn window_covering_all_joined_refreshes_report_cache() {
        let (_dir, db) = scratch_db();
        for i in 0..12 {
            let sha = format!("sha{i}");
            record_selection(&db, &result_selecting(&["tests/a.test.ts"]), "repo:1", &sha)
                .expect("sel");
            record_truth(&db, "repo:1", &sha, &[], Some(10), &[], Some(3)).expect("truth");
        }
        let cache_path = crate::sidecar_path(&db, REPORT_SUFFIX);

        compute_report(&db, 50).expect("default-window report");
        let cached: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cache_path).expect("cache must be written"),
        )
        .expect("parse cache");
        assert_eq!(cached["window"], 50, "default window refreshes the cache");

        compute_report(&db, 5).expect("slice report");
        let cached_after: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cache_path).expect("cache must survive"),
        )
        .expect("parse cache");
        assert_eq!(
            cached_after["window"], 50,
            "a genuine slice must not clobber the canonical cached report"
        );
    }

    /// P1 review: once history exceeds the default window, the default
    /// 50-window report must keep refreshing the cache — it clears the bar
    /// on its own and the disclosure carries its own window.
    #[test]
    fn default_window_keeps_refreshing_after_history_exceeds_window() {
        let (_dir, db) = scratch_db();
        for i in 0..51 {
            let sha = format!("sha{i}");
            record_selection(&db, &result_selecting(&["tests/a.test.ts"]), "repo:1", &sha)
                .expect("sel");
            record_truth(&db, "repo:1", &sha, &[], Some(10), &[], Some(3)).expect("truth");
        }
        let cache_path = crate::sidecar_path(&db, REPORT_SUFFIX);
        let _ = std::fs::remove_file(&cache_path);

        compute_report(&db, 50).expect("default-window report");
        let cached: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cache_path)
                .expect("cache must refresh for a sufficient windowed report"),
        )
        .expect("parse cache");
        assert_eq!(cached["window"], 50);
        assert_eq!(cached["n_joined"], 50);
    }

    /// P2 review: with out-of-order appends (parallel recorders), the join
    /// must pick the selection with the greatest eligible TIMESTAMP, not the
    /// last line in the file.
    #[test]
    fn join_picks_max_timestamp_not_append_order() {
        let (_dir, db) = scratch_db();
        let sel_path = crate::sidecar_path(&db, SELECTIONS_SUFFIX);
        let truth_path = crate::sidecar_path(&db, TRUTH_SUFFIX);

        // Filler pairs so the report clears the minimum-joined bar.
        let mut sels: Vec<String> = Vec::new();
        let mut truths: Vec<String> = Vec::new();
        for i in 0..12 {
            sels.push(
                serde_json::json!({
                    "ts": format!("2026-01-0{}", (i % 8) + 1), "repo_uid": "", "sha": format!("filler{i}"),
                    "changed_files": [], "selected_test_files": ["tests/a.rs"],
                    "status": "complete", "recommendation": "selection-usable"
                })
                .to_string(),
            );
            truths.push(
                serde_json::json!({
                    "ts": "2026-01-09T00:00:00Z", "repo_uid": "", "sha": format!("filler{i}"),
                    "failed_test_files": [], "total_test_files": 10,
                    "flaky_test_files": [], "reruns": 3
                })
                .to_string(),
            );
        }
        // The case under test: at "target" sha the NEWER selection is
        // appended BEFORE the older one (out-of-order append). The truth's
        // failure only exists in the newer selection's set — recall is 1.0
        // only if the join follows the max timestamp, not file position.
        sels.push(
            serde_json::json!({
                "ts": "2026-01-05T00:00:00Z", "repo_uid": "", "sha": "target",
                "changed_files": [], "selected_test_files": ["tests/new.rs"],
                "status": "complete", "recommendation": "selection-usable"
            })
            .to_string(),
        );
        sels.push(
            serde_json::json!({
                "ts": "2026-01-04T00:00:00Z", "repo_uid": "", "sha": "target",
                "changed_files": [], "selected_test_files": ["tests/old.rs"],
                "status": "complete", "recommendation": "selection-usable"
            })
            .to_string(),
        );
        truths.push(
            serde_json::json!({
                "ts": "2026-01-06T00:00:00Z", "repo_uid": "", "sha": "target",
                "failed_test_files": ["tests/new.rs"], "total_test_files": 10,
                "flaky_test_files": [], "reruns": 3
            })
            .to_string(),
        );
        std::fs::write(&sel_path, format!("{}\n", sels.join("\n"))).expect("write selections");
        std::fs::write(&truth_path, format!("{}\n", truths.join("\n"))).expect("write truths");

        let r = compute_report(&db, 0).expect("report");
        assert_eq!(r.n_joined, 13);
        assert_eq!(
            r.file_recall,
            Some(1.0),
            "join must follow max timestamp, not append order"
        );
    }

    /// P2 review: after a stale-lock takeover, the previous holder's Drop
    /// must not delete the successor's lock file.
    #[test]
    fn sidecar_lock_drop_preserves_successor_lock_after_takeover() {
        let (_dir, db) = scratch_db();
        let sidecar = crate::sidecar_path(&db, TRUTH_SUFFIX);
        let lock = SidecarLock::acquire(&sidecar).expect("acquire");
        let lock_path = sidecar.with_extension("lock");
        // A successor breaks the "stale" lock and installs its own token.
        std::fs::write(&lock_path, b"other-pid:0").expect("successor token");
        drop(lock);
        assert!(
            lock_path.exists(),
            "successor lock must survive the taken-over holder's Drop"
        );
        let _ = std::fs::remove_file(&lock_path);
    }
}
