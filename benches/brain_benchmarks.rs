//! Brain benchmark suite.
//!
//! Generates synthetic vaults with realistic wikilink density and runs
//! the brain's main workloads end-to-end. Defaults to a 1K-note vault
//! to keep the bench cycle under a minute; bump `BENCH_NOTES` to run
//! at larger scale (the architecture doc validation appendix calls for
//! 1K / 5K / 50K).
//!
//! Usage:
//!   cargo bench                          # default 1K scale
//!   BENCH_NOTES=5000 cargo bench         # 5K scale
//!   BENCH_NOTES=50000 cargo bench        # 50K scale
//!
//! What we measure:
//!   - cold_index: full markdown index of N notes from scratch
//!   - tantivy_search: BM25 query latency after the index is warm
//!   - ppr_compute: cost of a single compute_pagerank on the unified scope
//!   - brain_context_query: end-to-end PPR query with 3 seeds
//!
//! What we don't measure here:
//!   - File watcher event latency — requires real fs events, untestable
//!     deterministically in criterion (covered by the `#[ignore]`'d
//!     integration tests instead).
//!   - MCP tool round-trip — stdio framing dominates and adds noise.
//!
//! Results from a representative run are recorded in
//! docs/architecture/project-brain.md Appendix C.

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use nestweaver_engine::{HybridSearchConfig, build_brain_context_hybrid, index_markdown_directory};
use nestweaver_store::{GraphScope, GraphStore, TantivyIndex};
use tempfile::tempdir;

/// Number of notes to generate. Pulled from env so larger runs don't
/// require a recompile.
fn bench_notes() -> usize {
    std::env::var("BENCH_NOTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

/// Generate a synthetic vault on disk. Each note has 3 headings, a
/// preamble, frontmatter with a tag and a type, and ~3 wikilinks to
/// other notes — roughly the density of a real PKM vault per the
/// architecture doc.
fn synth_vault(n: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..n {
        // Three wikilink targets, modulo N to ensure they always
        // resolve. Distance scattered so PPR sees a non-trivial graph.
        let t1 = (i + 1) % n;
        let t2 = (i + 7) % n;
        let t3 = (i + 53) % n;
        let kind = match i % 4 {
            0 => "design",
            1 => "prd",
            2 => "meeting",
            _ => "note",
        };
        let body = format!(
            "---\n\
             type: {kind}\n\
             tags: [project, status/active]\n\
             ---\n\
             # Note {i}\n\
             \n\
             Preamble paragraph linking to [[note-{t1}]] and [[note-{t2}]].\n\
             \n\
             ## Background\n\
             Some background text mentioning [[note-{t3}]]. The Authenticator\n\
             class is referenced here for cross-domain matching.\n\
             \n\
             ## Decisions\n\
             - First decision\n\
             - Second decision\n\
             \n\
             ## Open Questions\n\
             More text with #inline-tag and prose about #design.\n",
            kind = kind,
            i = i,
            t1 = t1,
            t2 = t2,
            t3 = t3,
        );
        let path = root.join(format!("note-{i}.md"));
        std::fs::write(&path, body).unwrap();
    }
    (dir, root)
}

// ── bench: cold_index ──────────────────────────────────────────────────────

fn bench_cold_index(c: &mut Criterion) {
    let n = bench_notes();
    let mut group = c.benchmark_group("cold_index");
    group.sample_size(10); // index is expensive; keep iteration low
    group.bench_function(format!("notes={n}"), |b| {
        b.iter_with_setup(
            || {
                let (dir, root) = synth_vault(n);
                let db_dir = tempdir().unwrap();
                let db_path = db_dir.path().join("bench.lbug");
                // Hand both temp guards back so they outlive the closure.
                (dir, db_dir, root, db_path)
            },
            |(_dir, _db_dir, root, db_path)| {
                index_markdown_directory(&root, &db_path, "bench", "v").unwrap();
            },
        );
    });
    group.finish();
}

// ── bench: brain_context_query (after warm-up index + PPR) ─────────────────

fn bench_brain_context_query(c: &mut Criterion) {
    let n = bench_notes();
    let (_dir, root) = synth_vault(n);
    let db_dir = tempdir().unwrap();
    let db_path = db_dir.path().join("bench.lbug");
    index_markdown_directory(&root, &db_path, "bench", "v").unwrap();

    let store = GraphStore::open(&db_path).unwrap();
    // Warm PPR once so the query closure measures only the query path.
    store
        .compute_pagerank(0.85, 20, &GraphScope::unified())
        .unwrap();

    let seeds: Vec<String> = vec!["Background".into(), "Decisions".into(), "design".into()];

    c.bench_function(&format!("brain_context_query/notes={n}/seeds=3"), |b| {
        b.iter(|| {
            // No Tantivy here — measures pure-PPR end-to-end query latency.
            build_brain_context_hybrid(&store, &seeds, None, &HybridSearchConfig::default(), None, None)
                .unwrap()
        });
    });
}

// ── bench: tantivy_search ──────────────────────────────────────────────────

fn bench_tantivy_search(c: &mut Criterion) {
    let n = bench_notes();
    let (_dir, root) = synth_vault(n);
    let db_dir = tempdir().unwrap();
    let db_path = db_dir.path().join("bench.lbug");
    index_markdown_directory(&root, &db_path, "bench", "v").unwrap();
    let store = GraphStore::open(&db_path).unwrap();

    let tantivy_dir = db_dir.path().join("bench.tantivy");
    let tantivy = TantivyIndex::open_or_create(&tantivy_dir).unwrap();
    tantivy.reindex_from_store(&store).unwrap();

    c.bench_function(&format!("tantivy_search/notes={n}"), |b| {
        b.iter(|| tantivy.search("background design decisions", 20).unwrap());
    });
}

// ── bench: ppr_compute (unified scope) ─────────────────────────────────────

fn bench_ppr_compute(c: &mut Criterion) {
    let n = bench_notes();
    let (_dir, root) = synth_vault(n);
    let db_dir = tempdir().unwrap();
    let db_path = db_dir.path().join("bench.lbug");
    index_markdown_directory(&root, &db_path, "bench", "v").unwrap();
    let store = GraphStore::open(&db_path).unwrap();

    let mut group = c.benchmark_group("ppr_compute");
    group.sample_size(20);
    group.bench_function(format!("notes={n}/scope=unified/iters=20"), |b| {
        b.iter(|| {
            store
                .compute_pagerank(0.85, 20, &GraphScope::unified())
                .unwrap()
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_cold_index,
    bench_brain_context_query,
    bench_tantivy_search,
    bench_ppr_compute,
);
criterion_main!(benches);
