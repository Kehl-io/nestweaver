//! Guard: bench targets must not gate their bodies behind `cfg(not(test))`.
//!
//! Cargo compiles `harness = false` bench targets **with `--cfg test`**, so the
//! `#[cfg(not(test))]` + `#[cfg(test)] fn main() {}` idiom compiles the real
//! body out and runs the stub instead. Three of the four benches did exactly
//! that: they exited 0 having measured nothing, and `just bench` reported a
//! clean run. `test = false` does not prevent it — the behavior is cargo's.
//!
//! A silent no-op that presents as a passing benchmark run is worse than having
//! no benchmark, because it launders the absence of a measurement into apparent
//! success. This test fails if the idiom returns.

use std::path::Path;

/// Every `.rs` file under `benches/`, as (filename, contents).
fn bench_sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
    let mut sources: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("benches/ must exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            (name, body)
        })
        .collect();
    sources.sort();
    assert!(
        !sources.is_empty(),
        "benches/ contained no .rs files — this guard would silently pass"
    );
    sources
}

#[test]
fn no_bench_gates_its_body_behind_cfg_not_test() {
    let mut offenders = Vec::new();
    for (name, body) in bench_sources() {
        for (index, line) in body.lines().enumerate() {
            // Match the ATTRIBUTE, not prose: remove_repo_benchmarks.rs
            // documents this very idiom in its `//!` header, and that
            // explanation must stay.
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[cfg(not(test))]") || trimmed.starts_with("#[cfg(test)]") {
                offenders.push(format!("{name}:{}: {}", index + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "bench targets must not use cfg(test) gating — cargo compiles \
         `harness = false` benches with `--cfg test`, so these compile to \
         empty stubs that report success having measured nothing:\n  {}",
        offenders.join("\n  ")
    );
}

/// The stub `main` is the visible half of the same defect: with the gating
/// removed but an empty `fn main() {}` left behind, the bench still measures
/// nothing.
#[test]
fn no_bench_defines_an_empty_main() {
    let mut offenders = Vec::new();
    for (name, body) in bench_sources() {
        // Strip comments before matching: remove_repo_benchmarks.rs quotes
        // `#[cfg(test)] fn main() {}` verbatim in its header to explain the
        // defect, and that prose is not an offense.
        let code: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        if code.replace([' ', '\n'], "").contains("fnmain(){}") {
            offenders.push(name);
        }
    }
    assert!(
        offenders.is_empty(),
        "these benches define an empty `fn main() {{}}` and measure nothing: {offenders:?}"
    );
}
