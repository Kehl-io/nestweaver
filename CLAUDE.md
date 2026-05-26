# NestWeaver

## Build & Test

```sh
cargo build                                                 # build all crates
cargo build --release                                       # release binary
cargo test                                                  # run all tests
cargo test -p nestweaver-schema                             # test one crate
cargo clippy --all-targets --all-features -- -D warnings    # lint (zero warnings)
cargo fmt --all -- --check                                  # format check
cargo fmt --all                                             # format in place
```

## Run

```sh
# Index a repo and query it
nestweaver index --repo ./testdata/js
nestweaver context greet                 # task-focused subgraph via PPR
nestweaver context src/main.js           # seed from all symbols in a file
nestweaver search "greet"
nestweaver symbol "greet" --json
nestweaver impact "greet" --depth 3
nestweaver repo-map --token-budget 2000

# Multi-repo / instance config
nestweaver suggest-links --db ./all.lbug                                    # manifest deps + IDF name matching
nestweaver list-links --config ./nestweaver-instance.toml                   # declared [[links]]
nestweaver list-features --config ./nestweaver-instance.toml                # declared [[features]]
nestweaver context --feature device-pairing --config ./nestweaver-instance.toml --db ./all.lbug
```

Default database: `./nestweaver.lbug`. Override with `--db <path>`.

Sidecar files written alongside the database:
- `<db>.pagerank.json` — in-memory PageRank cache (written on `index`, loaded on open)
- `<db>.manifests.json` — parsed manifest data (package.json, go.mod, Cargo.toml, pyproject.toml, requirements.txt, composer.json, Gemfile, pubspec.yaml, Package.swift, *.csproj, build.gradle.kts, CMakeLists.txt)

## Architecture

Cargo workspace with 8 crates + root binary:

```
nestweaver/                     # CLI entry point (src/main.rs)
crates/
  nestweaver-schema/            # node/edge types, UIDs, confidence scoring, schema versioning
  nestweaver-parser/            # Tree-sitter + regex parsing for 16 languages
  nestweaver-resolver/          # cross-file import resolution with confidence scoring
  nestweaver-store/             # LadybugDB graph store, PageRank, hybrid search (BM25 + vector)
  nestweaver-storage/           # pluggable snapshot storage backends (local, S3, GitLab)
  nestweaver-engine/            # indexing pipeline, query dispatch, config, registry, snapshots, LLM pipelines
  nestweaver-mcp/               # optional MCP wrapper (feature-gated, delegates to engine)
  nestweaver-web/               # optional web UI and API backend (Axum + React)
```

### Dependency flow

```
schema          (zero internal deps)
  <- parser
  <- resolver
  <- store
storage         (zero internal deps)
       <- engine <- (parser, resolver, store, storage)
            <- mcp
            <- web
```

## Conventions

- Rust edition 2024, resolver 2
- `thiserror` for public errors in library crates; `anyhow` only in binary/engine
- `tracing` for structured logging; no `println!` in library crates
- No `unwrap()` or `expect()` in library code outside of tests
- Parameterized queries for all LadybugDB operations (no string interpolation)
- Conventional commits enforced by pre-commit hook (see `.commitlintrc.yml` for scopes)

## CI

- `ci.yml` — cargo fmt, clippy, test, gitleaks (on every PR and push to main)
- `release-please.yml` — automated releases, binary builds for x86_64/aarch64 x linux/darwin

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error |
| 2 | Not found (symbol, service) |
| 3 | Ambiguous match (multiple symbols with same name) |
| 4 | Unauthorized (pull) |
| 5 | Unavailable (pull) |
