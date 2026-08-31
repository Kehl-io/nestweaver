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

The authoritative list is `rules.scope-enum` in
[`.commitlintrc.yml`](.commitlintrc.yml) — read it there rather than from a copy
here, which is how the list below fell out of date in the first place. At time of
writing it holds: `schema`, `parser`, `resolver`, `store`, `storage`, `engine`,
`mcp`, `cli`, `ci`, `deps`, `docs`, `release`.

**The enum has drifted from practice.** History since `v8.0.0` contains commits
scoped `daemon`, `client`, `federation`, `proto`, `brain`, `context`,
`investigate`, `rankings`, `summaries` and `parity` — none of which are in the
enum. Either the hook is not enforced on every path, or it was bypassed. Before
adding a scope that is not in the enum, add it to `.commitlintrc.yml` in the same
commit; do not rely on the gap staying open.

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

# Task runner used by the recipes below (see `just --list`)
cargo install just
```

Keep `.cargo/config.toml` in place. It forces the Ladybug dependency to build
from source instead of a prebuilt archive, which avoids zstd link errors; the
initial native build can take several minutes. See
[INSTALL.md](INSTALL.md#build-from-source) for CMake, C++, OpenSSL, zstd,
`pkg-config`, and Protocol Buffers prerequisites.

#### Only one copy of zstd may be linked

`liblbug.a` vendors zstd, exports its symbols, and is linked `+whole-archive`,
so every binary already contains a complete libzstd. Rust code reaches that copy
through `nestweaver_store::zstd`.

Do not add the `zstd` crate as a dependency. It pulls in `zstd-sys`, which
compiles a **second** complete copy, and `rust-lld` — the default linker on
x86_64 Linux — then refuses to link anything, with dozens of duplicate `ZSTD_*`
symbols. That is what `-Wl,--allow-multiple-definition` used to suppress; the
flag never merged the copies, it only told the linker to pick one definition
silently while the other stayed in the binary.

CI passes `-C link-arg=-fuse-ld=mold` for speed; `mold` is optional locally.

### Check suite

Run all of these before submitting a PR:

```sh
cargo test                                                  # all tests
cargo clippy --workspace --all-targets -- -D warnings       # lint
cargo fmt --all -- --check                                  # formatting
```

The clippy line is `--workspace`, **not** `--all-features`: `--all-features`
reaches `metal = ["candle-core/metal", …]`, which pulls `objc2` and does not
compile on Linux — the same exemption the `just test-crate` section below
describes. This is the invocation CI runs and the one PR bodies report clean.

### Useful commands

```sh
# Run tests for a single crate — use the recipe, not bare `cargo test -p`
just test-crate nestweaver-parser

# Run a specific test (keep --all-features — see below)
cargo test -p nestweaver-store --all-features -- ranking::tests::pagerank

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

#### Why `just test-crate` and not `cargo test -p`

A bare `cargo test -p <crate>` resolves features for that package alone, while a
workspace run unifies them across every dependent. The result is a per-crate run
that covers less than you assume and still prints `ok`. Measured on `5e9e0f0`:

| crate | `cargo test -p` | `-p --all-features` | `cargo test --workspace` |
| --- | --- | --- | --- |
| `nestweaver-daemon` | 238 | **264** | **264** |
| `nestweaver-mcp` | 154 | **180** | **180** |

This cost two implementers and a reviewer real time during PR #245, each briefly
treating the gap as a discrepancy in the suite rather than a feature-set
difference.

`just test-crate` passes `--all-features`. That is deliberate, and not the same
as naming the features by hand: every feature unification can activate on a
package is one of that package's own features, so `--all-features` is provably a
**superset** of the unified set and can never cover less. Hand-maintained lists
are guesswork — `--features embed` is the obvious guess and leaves
`nestweaver-mcp` at 154, because what it actually needs is `daemon`
(`nestweaver-daemon` depends on it as `features = ["daemon"]`).

Two packages are exempt and run plain: `nestweaver-embed` and the root
`nestweaver`. Both reach `metal = ["candle-core/metal", …]`, which pulls `objc2`
and fails to compile on Linux.

Without `just` installed, the equivalent is `cargo test -p <crate>
--all-features` for any crate other than those two.

One consequence worth expecting either way: switching a working tree between
`-p` and `--workspace` re-resolves features, which re-fingerprints the build and
forces a full `lbug` C++ rebuild. Pick one shape per tree and stay with it.

### Code conventions

- `thiserror` for all public error types in library crates
- `anyhow` only in the binary and engine integration code
- `tracing` for logging, never `println!` in library crates
- No `unwrap()` or `expect()` in library code outside of tests
- Parameterized queries for all LadybugDB operations (no string interpolation)
