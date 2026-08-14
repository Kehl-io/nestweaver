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

# Packages where `--all-features` does not build off macOS. `nestweaver-embed`
# has `metal = ["candle-core/metal", ...]`, which pulls `objc2` and fails to
# compile on Linux (measured on 5e9e0f0). The root `nestweaver` package forwards
# the same feature via `metal = ["nestweaver-embed?/metal"]`, so it inherits the
# hazard. These fall back to a plain per-crate run.
no_all_features := "nestweaver nestweaver-embed"

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
    if [[ " {{no_all_features}} " == *" {{crate}} "* ]]; then
        echo "note: {{crate}} cannot take --all-features off macOS (metal/objc2) — running plain" >&2
        cargo test -p {{crate}}
    else
        cargo test -p {{crate}} --all-features
    fi

# Lint with clippy (zero warnings)
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Format code
fmt:
    cargo fmt --all

# Run all checks (what CI does)
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
