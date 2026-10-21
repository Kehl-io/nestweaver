# Default recipe: show available commands
default:
    @just --list

# Build all crates
build:
    cargo build

# Build release binary
release:
    cargo build --release

# Run all tests
test:
    cargo test

# Off macOS, `--all-features` does not build for any package with a `metal`
# feature: `nestweaver-embed` has `metal = ["candle-core/metal", ...]`, which
# pulls `objc2` and fails to compile on Linux (measured on 5e9e0f0), and
# `nestweaver` and `nestweaver-daemon` forward it. `test-crate` therefore passes
# every feature EXCEPT those that enable `metal`, directly or transitively
# (and the CI-only `ci-direct-tests`),
# rather than falling back to a plain run that tests less (264 vs 238 tests for
# `nestweaver-daemon`).

# Why `--all-features` rather than a hand-maintained per-crate feature list:
# every feature a workspace run can activate on a package is, by definition, one
# of that package's own features — so `--all-features` is always a SUPERSET of
# the unified set and can never silently cover less. A hand-maintained list can,
# and did: `--features embed` alone leaves `nestweaver-mcp` at 154 tests, because
# what it actually needs is `daemon` (nestweaver-daemon depends on it as
# `features = ["daemon"]`).
#
# Measured on 5e9e0f0 — bare `-p` / `--all-features` / `--workspace`:
#   nestweaver-daemon   238 / 264 / 264
#   nestweaver-mcp      154 / 180 / 180
#
# Keep the line below as the only comment directly above the recipe: `just
# --list` uses the immediately-preceding comment as its description.

# Test one crate with the features a workspace run would activate
test-crate crate:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(uname -s)" == Darwin ]]; then
        cargo test -p {{crate}} --all-features
        exit
    fi
    features=$(cargo metadata --no-deps --format-version 1 | python3 -c '
    import json, sys
    crate = sys.argv[1]
    package = next(p for p in json.load(sys.stdin)["packages"] if p["name"] == crate)
    features = package["features"]
    # ci-direct-tests is the CI-only daemon-bypass artifact (see CLAUDE.md).
    excluded = {"metal", "ci-direct-tests"} & features.keys()
    changed = True
    while changed:
        changed = False
        for name, enables in features.items():
            if name not in excluded and any(
                item == "metal" or item.endswith("/metal") or item in excluded
                for item in enables
            ):
                excluded.add(name)
                changed = True
    print(",".join(sorted(set(features) - excluded - {"default"})))
    ' {{crate}})
    if [[ -n "$features" ]]; then
        echo "note: off macOS, testing {{crate}} with all features except metal: $features" >&2
        cargo test -p {{crate}} --features "$features"
    else
        cargo test -p {{crate}}
    fi

# Lint with clippy (zero warnings) -- same invocation CI runs
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Format code
fmt:
    cargo fmt --all

# Run all checks (what CI does)
#
# `lint` deliberately does NOT pass `--all-features`: as noted above,
# `nestweaver` and `nestweaver-embed` forward `metal`, which pulls
# `objc2` and does not compile off macOS -- so an `--all-features` clippy run
# is not portable, and CI does not use one either
# (.github/workflows/ci.yml runs `cargo clippy --workspace --all-targets`).
check: fmt-check lint test

# Start web UI in development mode
dev:
    cargo run -- ui --no-open

# Start web UI with frontend hot-reload (requires two terminals or use & )
dev-full:
    @echo "Starting backend on port 3000..."
    @echo "Run 'cd crates/nestweaver-web/frontend && npm run dev' in another terminal for frontend hot-reload"
    cargo run -- ui --no-open

# Index the test data repository
index-testdata:
    cargo run -- index --repo ./testdata/js

# Run MCP server (for testing with Claude Code)
mcp:
    cargo run -- mcp

# Generate and view repo map
repo-map:
    cargo run -- repo-map --token-budget 4000

# Run benchmarks
bench:
    cargo bench

# Clean build artifacts
clean:
    cargo clean

# Install the binary locally
install:
    cargo install --path .

# Frontend: install dependencies
frontend-install:
    cd crates/nestweaver-web/frontend && npm install

# Frontend: dev server with hot reload
frontend-dev:
    cd crates/nestweaver-web/frontend && npm run dev

# Frontend: production build
frontend-build:
    cd crates/nestweaver-web/frontend && npm run build

# Frontend: lint
frontend-lint:
    cd crates/nestweaver-web/frontend && npm run lint
