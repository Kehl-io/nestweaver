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

# Test a specific crate
test-crate crate:
    cargo test -p {{crate}}

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
