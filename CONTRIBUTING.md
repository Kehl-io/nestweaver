# Contributing to NestWeaver

## Reporting Issues

Open a [GitHub issue](https://github.com/Kehl-io/nestweaver/issues) with:

- What you expected vs. what happened
- Steps to reproduce
- NestWeaver version (`nestweaver --version`)
- OS and Rust version

For security vulnerabilities, see [SECURITY.md](.github/SECURITY.md) instead.

## Submitting Changes

1. Fork the repo and create a branch from `main`
2. Make your changes with tests
3. Run the full check suite (see below)
4. Open a pull request against `main`

PRs should be focused — one feature or fix per PR. If a change touches
multiple crates, that's fine as long as it's one logical change.

## Architecture

See [CLAUDE.md](CLAUDE.md) for the crate dependency diagram and conventions.
The key rule: `schema` and `storage` have zero internal dependencies.
Everything else depends on `schema`. Only `engine` depends on all crates.

## Commit Convention

All commits must follow [Conventional Commits](https://www.conventionalcommits.org/).

Format: `<type>(<scope>): <description>`

### Types

| Type | When to use |
|------|-------------|
| `feat` | New feature or capability |
| `fix` | Bug fix |
| `refactor` | Code change that doesn't add features or fix bugs |
| `test` | Adding or updating tests |
| `docs` | Documentation only |
| `chore` | Build process, tooling, or dependency updates |
| `ci` | CI/CD configuration |
| `perf` | Performance improvement |

### Scopes

| Scope | Crate / Area |
|-------|-------------|
| `schema` | nestweaver-schema |
| `parser` | nestweaver-parser |
| `resolver` | nestweaver-resolver |
| `store` | nestweaver-store |
| `storage` | nestweaver-storage |
| `engine` | nestweaver-engine |
| `mcp` | nestweaver-mcp |
| `cli` | Binary / CLI (src/main.rs) |
| `ci` | CI/CD workflows |
| `deps` | Dependency updates |
| `docs` | Documentation only |
| `release` | Release automation |

### Examples

```
feat(parser): add Go interface satisfaction detection
fix(resolver): handle circular import cycles in Python
test(store): add concurrent read safety test
docs(cli): improve --help text for impact command
```

## Development

### Setup

```sh
# Clone and install native build prerequisites (see INSTALL.md for each OS)
git clone https://github.com/Kehl-io/nestweaver.git
cd nestweaver
cargo build

# Install pre-commit hooks (requires pre-commit and Node.js for commitlint)
pre-commit install
pre-commit install --hook-type commit-msg
```

Keep `.cargo/config.toml` in place. It forces the Ladybug dependency to build
from source instead of a prebuilt archive, which avoids zstd link errors; the
initial native build can take several minutes. See
[INSTALL.md](INSTALL.md#build-from-source) for CMake, C++, OpenSSL, zstd,
`pkg-config`, and Protocol Buffers prerequisites.

### Check suite

Run all of these before submitting a PR:

```sh
cargo test                                                  # all tests
cargo clippy --all-targets --all-features -- -D warnings    # lint
cargo fmt --all -- --check                                  # formatting
```

### Useful commands

```sh
# Run tests for a single crate
cargo test -p nestweaver-parser

# Run a specific test
cargo test -p nestweaver-store -- ranking::tests::pagerank

# Build release binary
cargo build --release

# Index a test repo and query it
nestweaver index --repo ./testdata/js
nestweaver context greet              # task-focused subgraph via PPR
nestweaver context src/main.js        # seed from all symbols in a file
nestweaver search "greet"
nestweaver symbol "greet"

# Multi-repo commands
nestweaver suggest-links --db ./all.lbug                                   # detect manifest deps + shared symbols
nestweaver list-links --config ./nestweaver-instance.toml
nestweaver list-features --config ./nestweaver-instance.toml
nestweaver context --feature <name> --config ./nestweaver-instance.toml --db ./all.lbug
```

### Code conventions

- `thiserror` for all public error types in library crates
- `anyhow` only in the binary and engine integration code
- `tracing` for logging, never `println!` in library crates
- No `unwrap()` or `expect()` in library code outside of tests
- Parameterized queries for all LadybugDB operations (no string interpolation)
