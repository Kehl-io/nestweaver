//! Indexing pipeline benchmark suite.
//!
//! Measures cold and warm indexing performance against a synthetic TypeScript
//! repository. Cold indexing runs with `force=true` on a fresh DB; warm noop
//! runs with `force=false` when the filemeta sidecar already matches every
//! file, so the parser is bypassed entirely.
//!
//! Usage:
//!   cargo bench --bench index_benchmarks              # run all
//!   cargo bench --bench index_benchmarks -- --test    # compile-check only
//!
//! Scaling:
//!   BENCH_FILES=500 cargo bench --bench index_benchmarks

#[cfg(not(test))]
use std::path::PathBuf;
#[cfg(not(test))]
use std::sync::Once;

#[cfg(not(test))]
use criterion::{Criterion, criterion_group, criterion_main};
#[cfg(not(test))]
use nestweaver_engine::index_directory_with_options;
#[cfg(not(test))]
use tempfile::tempdir;

#[cfg(not(test))]
static INIT_TRACING: Once = Once::new();

#[cfg(not(test))]
fn init_tracing() {
    INIT_TRACING.call_once(|| {
        use tracing_subscriber::fmt::format::FmtSpan;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "nestweaver_engine=info".parse().unwrap()),
            )
            .with_span_events(FmtSpan::CLOSE)
            .with_target(false)
            .try_init();
    });
}

/// Number of TypeScript files to generate. Pull from env to avoid recompile.
#[cfg(not(test))]
fn bench_files() -> usize {
    std::env::var("BENCH_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

/// Generate a synthetic TypeScript repository on disk.
///
/// Each file exports a class with a constructor and two methods that call each
/// other, producing a realistic symbol + reference density. Files in `src/`
/// have varying subdirectories to exercise service-node grouping.
#[cfg(not(test))]
fn synth_ts_repo(n: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");

    // Create subdirectory layout: src/controllers, src/services, src/models
    let subdirs = ["controllers", "services", "models"];
    for sub in &subdirs {
        std::fs::create_dir_all(root.join("src").join(sub)).unwrap();
    }

    for i in 0..n {
        let sub = subdirs[i % subdirs.len()];
        let next = (i + 1) % n;
        let content = format!(
            r#"// Auto-generated synthetic file {i}
import {{ Item{next} }} from '../{next_sub}/item{next}';

export class Item{i} {{
    private value: number;

    constructor(initial: number) {{
        this.value = initial;
    }}

    process(): number {{
        const dep = new Item{next}(this.value);
        return this.transform(dep.fetch());
    }}

    transform(input: number): number {{
        return input * 2 + this.value;
    }}

    fetch(): number {{
        return this.value;
    }}
}}

export function create{i}(n: number): Item{i} {{
    return new Item{i}(n);
}}
"#,
            i = i,
            next = next,
            next_sub = subdirs[next % subdirs.len()],
        );
        let file_path = root.join("src").join(sub).join(format!("item{i}.ts"));
        std::fs::write(&file_path, content).unwrap();
    }

    (dir, root)
}

// ── bench: cold_index ─────────────────────────────────────────────────────

#[cfg(not(test))]
fn bench_cold_index(c: &mut Criterion) {
    init_tracing();
    let n = bench_files();
    let mut group = c.benchmark_group("code_index");
    group.sample_size(10); // indexing is expensive; keep iteration count low

    group.bench_function(format!("cold/files={n}"), |b| {
        b.iter_with_setup(
            || {
                let (repo_dir, repo_path) = synth_ts_repo(n);
                let db_dir = tempdir().unwrap();
                let db_path = db_dir.path().join("bench.lbug");
                // Return guards so they live until the iteration is done.
                (repo_dir, db_dir, repo_path, db_path)
            },
            |(_repo_dir, _db_dir, repo_path, db_path)| {
                index_directory_with_options(
                    &repo_path,
                    &db_path,
                    "bench",
                    "https://example.com/bench-repo",
                    "abc123",
                    true, // force = true → cold index, ignore filemeta
                    None,
                )
                .unwrap()
            },
        );
    });

    group.finish();
}

// ── bench: warm_noop ──────────────────────────────────────────────────────

#[cfg(not(test))]
fn bench_warm_noop(c: &mut Criterion) {
    init_tracing();
    let n = bench_files();
    let mut group = c.benchmark_group("code_index");
    group.sample_size(10);

    // Pre-build the repo and run one cold index to populate the filemeta sidecar.
    let (repo_dir, repo_path) = synth_ts_repo(n);
    let db_dir = tempdir().unwrap();
    let db_path = db_dir.path().join("bench.lbug");

    index_directory_with_options(
        &repo_path,
        &db_path,
        "bench",
        "https://example.com/bench-repo",
        "abc123",
        false,
        None,
    )
    .expect("warm-up cold index");

    group.bench_function(format!("warm_noop/files={n}"), |b| {
        b.iter(|| {
            // force=false + unchanged files → tiered detection skips all parsing
            index_directory_with_options(
                &repo_path,
                &db_path,
                "bench",
                "https://example.com/bench-repo",
                "abc123",
                false,
                None,
            )
            .unwrap()
        });
    });

    // Keep guards alive for the duration of the benchmark group.
    drop(repo_dir);
    drop(db_dir);

    group.finish();
}

#[cfg(not(test))]
criterion_group!(benches, bench_cold_index, bench_warm_noop);
#[cfg(not(test))]
criterion_main!(benches);

#[cfg(test)]
fn main() {}
