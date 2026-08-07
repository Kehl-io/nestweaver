//! Remove-repo hub-degree regression benchmark.
//!
//! Measures the cost of the daemon's remove-repo sequence
//! (`bulk_delete_repo_files_and_symbols` → `clear_repo_derived_nodes` →
//! `delete_repo_node`, crates/nestweaver-daemon/src/server.rs:1757-1759)
//! against a synthetic graph with ONE high-degree hub: a single Service node
//! (`svc:hub`) holding `degree` `SERVICE_HAS_SYMBOL` edges (FROM Service TO
//! Symbol, crates/nestweaver-store/src/db.rs:1494). Each deleted Symbol retires
//! one incident edge on the hub, so removal cost scales with hub degree.
//!
//! The Tantivy finalize step of the real sequence is intentionally omitted:
//! the degree-dependent term lives in the lbug `DETACH DELETE`s, not in the
//! search index.
//!
//! ── Refuted claims from the incident profile (kept as the record) ─────────
//!
//! 1. "Not a full node-group scan": `randomLookup=true` is supplied and
//!    honored; one CSR header entry is read; scans are bounded by
//!    `getCSRLength(offsetInGroup)`. A genuine group-wide scan (131,072
//!    rows/group) would take hours, not the observed time.
//! 2. "Both O(n²) and O(n·m) where m = node-group size are wrong": the true
//!    cost is quadratic in node DEGREE, not in relationships deleted —
//!    Θ(Σ_v deg(v)² + Σ_(u→v) deg_bwd(v)).
//! 3. "The sampling was right; the inference from it was not": the 93% stack
//!    attribution (3326/3579 samples on `CSRNodeGroup::scan`) is fully
//!    consistent with the correct reading — a stack sampler cannot
//!    distinguish degree-bounded from group-bounded scanning.
//!
//! `STORAGE_DIRECTION` is unusable as a fix because of the backward traversal
//! `<-[:SERVICE_HAS_SYMBOL]-(svc:Service)` in `tested_service_uids`
//! (crates/nestweaver-store/src/read.rs:610).
//!
//! Usage:
//!   cargo bench --bench remove_repo_benchmarks              # default scale
//!   cargo bench --bench remove_repo_benchmarks -- --test    # compile-check only
//!
//! Scaling:
//!   BENCH_HUB_DEGREES=1000,8700 cargo bench --bench remove_repo_benchmarks
//!   BENCH_HUB_DEGREES=1000,8700,86800 ...                   # incident scale (slow!)
//!
//! NOTE: unlike the older benches in this directory, this file deliberately
//! does NOT gate its code behind `#[cfg(not(test))]`. Cargo compiles
//! `harness = false` bench targets with `--cfg test`, so the
//! `cfg(not(test))` + `#[cfg(test)] fn main() {}` idiom compiles to an empty
//! stub binary that silently measures nothing. The `[[bench]]` entry sets
//! `test = false`, so this target is only ever built by `cargo bench` and the
//! criterion-generated `main` is always the right one.
//!
//! The sanity bound below keys off the SMALLEST degree passed. If you run
//! only incident-scale degrees (e.g. `BENCH_HUB_DEGREES=86800`), the 60s
//! bound applies to that point and WILL fail on unfixed code — that slowness
//! is the regression being measured, so treat the failure as the data, not
//! as bench breakage.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use nestweaver_schema::{File, Repo, Service, Symbol, SymbolKind, Visibility};
use nestweaver_store::GraphStore;
use tempfile::tempdir;

/// Symbols per synthetic File node (mirrors a realistic file's symbol count).
const SYMBOLS_PER_FILE: usize = 28;

/// Degrees at or above this run as a single manual `Instant` measurement
/// instead of through criterion: criterion's sample-size floor would repeat
/// an iteration that already takes minutes on unfixed code, turning one data
/// point into hours.
const MANUAL_DEGREE_THRESHOLD: usize = 20_000;

/// Generous sanity bound: removal at the smallest measured degree must finish
/// well under a minute. Fails only when catastrophically broken, never on
/// noise.
const SANITY_BOUND: Duration = Duration::from_secs(60);

static INIT_TRACING: Once = Once::new();

fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "nestweaver_store=info".parse().unwrap()),
            )
            .with_target(false)
            .try_init();
    });
}

/// Recorded per-iteration removal durations, keyed by hub degree.
fn timings() -> &'static Mutex<BTreeMap<usize, Vec<Duration>>> {
    static TIMINGS: OnceLock<Mutex<BTreeMap<usize, Vec<Duration>>>> = OnceLock::new();
    TIMINGS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Hub degrees to measure. Pull from env to avoid recompile; the 86,800
/// incident-scale point is opt-in because it is very slow on unfixed code.
fn bench_hub_degrees() -> Vec<usize> {
    std::env::var("BENCH_HUB_DEGREES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect::<Vec<usize>>()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![1000, 8700])
}

/// Minimal Symbol construction, inlined from the private `make_symbol` test
/// helper at crates/nestweaver-store/src/lib.rs:79-100.
fn make_symbol(uid: &str, name: &str, repo_uid: &str, file_path: &str) -> Symbol {
    Symbol {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: SymbolKind::Function,
        repo_uid: repo_uid.to_string(),
        file_path: file_path.to_string(),
        start_line: 10,
        end_line: 25,
        signature: format!("fn {name}()"),
        summary: Some(format!("Does {name} things")),
        content_hash: "contenthash".to_string(),
        embedding: None,
        pagerank_score: Some(0.5),
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    }
}

/// Build a committed synthetic graph: one Repo, one Service hub (`svc:hub`),
/// `degree` Symbols spread across `degree/28` Files, and `degree`
/// SERVICE_HAS_SYMBOL edges from the hub — one edge per symbol.
///
/// Returns the tempdir guard, the store, and the repo UID. The caller must
/// keep the guard alive for as long as the store is used.
///
/// Uses a tempdir FILE db (not `in_memory`) for fidelity to the incident.
fn synth_hub_graph(degree: usize) -> (tempfile::TempDir, GraphStore, String) {
    let dir = tempdir().unwrap();
    let db_path: PathBuf = dir.path().join("bench.lbug");
    let store = synth_hub_graph_at(&db_path, degree);
    (dir, store, "repo:bench".to_string())
}

/// Populate a fresh DB at `db_path` with the synthetic hub graph.
fn synth_hub_graph_at(db_path: &std::path::Path, degree: usize) -> GraphStore {
    let store = GraphStore::create(db_path).unwrap();
    let repo_uid = "repo:bench";

    store
        .insert_repo(&Repo {
            uid: repo_uid.to_string(),
            url: "https://example.com/bench-repo".to_string(),
            indexed_sha: "abc123".to_string(),
            staleness_commits_behind: 0,
            instance_id: "inst-1".to_string(),
            name: None,
            root_path: None,
        })
        .unwrap();

    let n_files = degree.div_ceil(SYMBOLS_PER_FILE);
    let files: Vec<File> = (0..n_files)
        .map(|i| File {
            uid: format!("file:{i}"),
            path: format!("src/file{i}.rs"),
            repo_uid: repo_uid.to_string(),
            content_hash: "hash1".to_string(),
        })
        .collect();
    let symbols: Vec<Symbol> = (0..degree)
        .map(|i| {
            let file = &files[i / SYMBOLS_PER_FILE];
            make_symbol(
                &format!("sym:{i}"),
                &format!("sym{i}"),
                repo_uid,
                &file.path,
            )
        })
        .collect();
    let services = vec![Service {
        uid: "svc:hub".to_string(),
        name: "hub".to_string(),
        repo_uid: repo_uid.to_string(),
        summary: None,
        summary_hash: None,
        embedding: None,
    }];

    let repo_file_edges: Vec<(&str, &str)> =
        files.iter().map(|f| (repo_uid, f.uid.as_str())).collect();
    let file_symbol_edges: Vec<(&str, &str)> = (0..degree)
        .map(|i| {
            (
                files[i / SYMBOLS_PER_FILE].uid.as_str(),
                symbols[i].uid.as_str(),
            )
        })
        .collect();
    let service_symbol_edges: Vec<(&str, &str)> = symbols
        .iter()
        .map(|s| ("svc:hub", s.uid.as_str()))
        .collect();

    // ONE bulk_index_write call = one committed transaction. The graph MUST be
    // fully committed before the timed removal: uncommitted rels take a
    // local-storage branch that shows no quadratic term, so building and
    // deleting in one transaction would silently measure nothing.
    store
        .bulk_index_write(
            &files,
            &symbols,
            &repo_file_edges,
            &file_symbol_edges,
            &services,
            &service_symbol_edges,
        )
        .unwrap();

    store
}

/// The timed remove-repo sequence (mirrors the daemon, minus Tantivy
/// finalize): bulk file/symbol delete, derived-node clear, repo node delete.
fn remove_repo_sequence(store: &GraphStore, repo_uid: &str) {
    store.bulk_delete_repo_files_and_symbols(repo_uid).unwrap();
    store.clear_repo_derived_nodes(repo_uid).unwrap();
    store.delete_repo_node(repo_uid).unwrap();
}

// ── bench: remove_repo ────────────────────────────────────────────────────

fn bench_remove_repo(c: &mut Criterion) {
    init_tracing();
    let degrees = bench_hub_degrees();
    let mut group = c.benchmark_group("remove_repo");
    group.sample_size(10); // removal is expensive; keep iteration count low

    for &degree in &degrees {
        if degree >= MANUAL_DEGREE_THRESHOLD {
            // Single manual run: criterion's sample floor would make the
            // incident-scale point run for hours on unfixed code.
            let (dir, store, repo_uid) = synth_hub_graph(degree);
            let start = Instant::now();
            remove_repo_sequence(&store, &repo_uid);
            let elapsed = start.elapsed();
            println!("remove_repo/hub_degree={degree} (manual, 1 run): {elapsed:?}");
            timings()
                .lock()
                .unwrap()
                .entry(degree)
                .or_default()
                .push(elapsed);
            drop(store);
            drop(dir);
            continue;
        }

        group.bench_function(format!("hub_degree={degree}"), |b| {
            b.iter_with_setup(
                || {
                    // Setup (untimed): build a fresh committed graph so each
                    // iteration removes a fresh copy.
                    synth_hub_graph(degree)
                },
                |(dir, store, repo_uid)| {
                    let start = Instant::now();
                    remove_repo_sequence(&store, &repo_uid);
                    let elapsed = start.elapsed();
                    timings()
                        .lock()
                        .unwrap()
                        .entry(degree)
                        .or_default()
                        .push(elapsed);
                    drop(store);
                    drop(dir)
                },
            );
        });
    }

    group.finish();
    report();
}

/// Print per-degree timings, the scaling ratio, and its interpretation; then
/// apply the sanity bound.
fn report() {
    let timings = timings().lock().unwrap();
    if timings.is_empty() {
        return;
    }

    println!("\n── remove-repo hub-degree report ──────────────────────────────");
    let mut means: Vec<(usize, Duration)> = Vec::new();
    for (degree, samples) in timings.iter() {
        let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
        means.push((*degree, mean));
        println!(
            "hub_degree={degree}: mean {mean:?} over {} run(s)",
            samples.len()
        );
    }

    for pair in means.windows(2) {
        let (d_lo, t_lo) = pair[0];
        let (d_hi, t_hi) = pair[1];
        let degree_ratio = d_hi as f64 / d_lo as f64;
        let time_ratio = t_hi.as_secs_f64() / t_lo.as_secs_f64();
        println!(
            "ratio time({d_hi})/time({d_lo}) = {time_ratio:.1}× \
             (degree ratio {degree_ratio:.1}×)"
        );
        println!(
            "interpretation: ≈{degree_ratio:.1}× is linear; \
             ≈{:.0}× is quadratic-in-degree",
            degree_ratio * degree_ratio
        );
    }

    // Sanity bound only: the smallest measured degree must remove in well
    // under a minute. Fails when catastrophically broken, never on noise.
    let (d_min, t_min) = means[0];
    assert!(
        t_min < SANITY_BOUND,
        "remove-repo at hub_degree={d_min} took {t_min:?}, exceeding the {SANITY_BOUND:?} sanity bound"
    );
}

criterion_group!(benches, bench_remove_repo);
criterion_main!(benches);
