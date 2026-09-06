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

## Review rule: sibling gaps

**When the same property must hold in two places, make the second place CALL
the first, or make the property a TYPE. Do not write it twice and remember to
check the twin.**

"Remember to check the twin" has failed at least ten times in this repo. Every
instance of this class that has stayed fixed was fixed structurally; every one
patched by mirroring came back. This is a review rule rather than a lint
because the two legs it covers are not syntactically expressible — "these two
functions should agree" is not a pattern a checker can match, and a semantic
divergence between routes is what the parity harness exists for. Where the
property *can* be checked mechanically it already is (see the CLI/MCP bounds
sweep in `src/main.rs`, which probes every declared bound through the real
parser).

### The canon

Worked examples that shipped, so this is a canon rather than an exhortation:

| Example | The shape |
| --- | --- |
| `render_cost` | The CLI's twin was DELETED and the function made `pub` so the CLI calls it. The copy had drifted to the wrong branch and was charging a concise response at the detailed rate. |
| `property_matches` | The CLI calls the MCP predicate instead of restating it. |
| `repo_head` | One implementation behind both `stale-check` routes. |
| `DbWriteLease` | `#[must_use]` moved onto the TYPE rather than each producer, so no caller can forget it. |
| `provenance_seam::Unstamped` | The field is private to its module, so `stamp` is the only way to construct the stamped form — the property is unforgeable rather than remembered. |
| `PublicationRootLock` | Destructive routes take the lock BY REFERENCE and continue through its canonical root, so a route cannot hold a lock that does not cover what it deletes. |
| `render_optional_count`, `MAX_IMPACT_DEPTH` | Shared renderer and shared constant, rather than two spellings of one number. |

### The caveat that saves you from a regression

**"Sibling A has a guard sibling B lacks" does not always mean B should take it.**
Blindly mirroring a sibling's write-gate onto `brain_memory_consolidate` would
have made every dry-run PREVIEW queue behind in-flight writes — a guard that is
correct for the mutating path and wrong for the read-only one. Establish that
the property genuinely belongs in both places before unifying; sometimes the
honest answer is that the twins legitimately differ, and then the divergence
belongs in a comment rather than in a shared function.

## Architecture

See [CLAUDE.md](CLAUDE.md) for the full crate dependency diagram and
conventions — read the diagram there rather than a paraphrase here, which is
what let this section fall out of sync with it. The short version: `schema`,
`storage`, and `algorithms` have zero internal dependencies; `algorithms` is
the WASM-compatible pure-compute layer, so `nestweaver-wasm` depends on it
alone and, notably, does *not* depend on `schema`. No single crate depends on
all the others — `engine` depends on `schema`/`parser`/`resolver`/`store`/
`storage`/`algorithms` (plus `embed` behind a feature); the root `nestweaver`
binary is the one that aggregates almost everything, transitively.

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
here, which is how the list below fell out of date once already. As of
2026-08-30 it holds 24 scopes: the original `schema`, `parser`, `resolver`,
`store`, `storage`, `engine`, `mcp`, `cli`, `ci`, `deps`, `docs`, `release`,
plus `brain`, `client`, `context`, `daemon`, `federation`, `impact`,
`investigate`, `parity`, `proto`, `queries`, `rankings`, `summaries` — added to
match scopes that were already in use in commit history since `v8.0.0`.

**The enum drifted from practice once already, because commitlint runs only as
a pre-commit hook, not in CI** — a scope outside the enum only ever fails
locally, and only for someone with the hook installed. That gap is why the
enum could describe a stale set of scopes for a while without anything
red across it. Before adding a scope that is not in the enum, add it to
`.commitlintrc.yml` in the same commit; do not rely on the gap staying open.

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
nestweaver context simple.js          # seed from all symbols in a file
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

## Release gate

Release Please intentionally uses the workflow's `GITHUB_TOKEN`. GitHub may
suppress the resulting `pull_request` event entirely or hold its workflow in
`action_required` with zero jobs. This repository uses that as an explicit gate
rather than adding another long-lived credential. After the release workflow
has finished synchronizing the lockfile and marked the PR ready, a maintainer
must approve a held CI run or explicitly dispatch `CI` from the release PR's
latest branch. For example, after checking the current PR head:

```sh
PR=123
HEAD_REF=$(gh pr view "$PR" --json headRefName --jq .headRefName)
HEAD_SHA=$(gh pr view "$PR" --json headRefOid --jq .headRefOid)
gh workflow run ci.yml --ref "$HEAD_REF"
# Before merge, require the successful Required CI check to report HEAD_SHA.
printf 'expected release PR head: %s\n' "$HEAD_SHA"
```

Do not approve, dispatch, or merge against an earlier head; another automation
commit dismisses approval and makes that CI evidence stale.

Protect `main` with a ruleset that requires pull requests, a CODEOWNER approval,
and the **Required CI** check from the GitHub Actions app. Require the branch to
be up to date, dismiss approvals after every push, and give neither admins nor
the release automation actor a bypass. Conditional jobs such as Rustfmt and
Cold Metal must not be named individually in the ruleset: `Required CI` fails
closed when any applicable non-advisory job is failed, cancelled, missing, or
unexpectedly skipped. A workflow held in `action_required`, a workflow that
creates zero jobs, and an invalid workflow all leave `Required CI` absent, so
the ruleset must treat the missing check as blocking. If repository Actions
policy still holds the trusted automation actor for approval, approve that run
explicitly; never merge around the absent check.

Also protect `v*` tags from update and deletion, and restrict creation to the
release automation identity. Require full-SHA action pins in repository Actions
settings. Create a protected `release` environment, allow only `main`, require a
maintainer reviewer, and expose `NPM_TOKEN` only there. These repository rules,
environment protections, and Actions approval settings live in GitHub; the
workflow file cannot install them by itself. The dry-run canary refuses to pass
unless the applied branch rules include PR review, CODEOWNER review, strict
up-to-date checks, and the `Required CI` check from a specific GitHub App.

The public release is intentionally last. Release Please first creates a
private draft without a tag. The workflow then checks out the exact release
SHA, waits for that SHA's successful `CI` push run and `Required CI` job, builds
all four targets, checksums each archive, extracts and smoke-tests the consumer
layout, creates provenance attestations, and validates the exact eight-file
bundle. Before publication, a protected-environment job proves Cargo, manifest,
npm, and tag versions agree, validates the exact npm tarball contents and
integrity, authenticates to npm, and observes whether the immutable version is
already present. The publication job then replaces assets through the validated
release ID, compares every remote size and SHA-256 digest, and downloads each
exact asset ID for a byte-for-byte check against the verified local files. Only
then does it publish the draft and create the tag. npm publication is idempotent
and depends on that completed transition; immediately before npm publication it
downloads the public assets again and rechecks the four checksum pairs and
provenance. A failed or absent target leaves only a private draft and cannot
publish npm.

Run the lightweight local policy checks with:

```sh
bash scripts/verify-required-ci.sh --self-test
bash scripts/verify-release-bundle.sh --self-test     # GNU coreutils only
bash scripts/verify-release-package.sh --self-test    # GNU coreutils only
bash -n scripts/observe-release-visibility.sh scripts/verify-release-canary-pr.sh
```

**Two of those do not run on macOS.** `verify-release-bundle.sh` and
`verify-release-package.sh` use `find -printf`, which is a GNU extension that
BSD `find` does not implement, so on macOS they exit 1 with
`find: -printf: unknown primary or operator` before testing anything. That is a
portability gap in the scripts, not a failure you introduced — they run on
ubuntu in `Required CI` and in the release workflow, which is the only place
they are load-bearing. `verify-required-ci.sh --self-test` and the `bash -n`
checks work everywhere.

The release workflow also has positive and negative controls. They build and
attest artifacts but never call Release Please, create a tag/release, or publish
npm. Each run creates a temporary automation-authored canary branch and PR,
observes that the real ruleset blocks it while `Required CI` is absent or held
in `action_required`, then closes the PR and deletes the branch. It also queries
GitHub tags, public/private releases, and the exact synthetic npm version before
and after the matrix; the evidence is observed state, not a skipped-job claim:

```sh
SHA=$(git rev-parse origin/main)
gh workflow run release-please.yml --ref main -f operation=dry-run -f candidate_sha="$SHA" -f fault_mode=none
gh workflow run release-please.yml --ref main -f operation=dry-run -f candidate_sha="$SHA" -f fault_mode=fail -f fault_target=x86_64-unknown-linux-gnu
gh workflow run release-please.yml --ref main -f operation=dry-run -f candidate_sha="$SHA" -f fault_mode=omit -f fault_target=aarch64-apple-darwin
```

Preserve the `release-gate-evidence-*` artifact from each run. The `none` run
must validate all eight files. The `fail` and `omit` runs must show that an
incomplete matrix is rejected while the exact synthetic tag, release, and npm
version remain absent. The evidence must also contain the canary PR's blocked
merge state and absent/`action_required` workflow observation.

A private draft can be resumed only from the completed release workflow run
whose artifacts and attestations belong to the same exact SHA. Resume deletes
and deterministically replaces only that release ID's private assets. If that
source run is incomplete, delete only the explicitly bound private draft and
then repair/re-run Release Please; cleanup refuses a public release or visible
tag. If GitHub publication succeeded but npm did not, the npm-only recovery
revalidates the public release, exact tag/SHA, eight remote digests, package
contents, and registry identity before publishing or accepting an already
identical immutable version:

```sh
gh workflow run release-please.yml --ref main -f operation=resume \
  -f candidate_sha="$SHA" -f release_id="$RELEASE_ID" \
  -f release_tag="$TAG" -f source_run_id="$SOURCE_RUN_ID"
gh workflow run release-please.yml --ref main -f operation=cleanup-draft \
  -f candidate_sha="$SHA" -f release_id="$RELEASE_ID" -f release_tag="$TAG"
gh workflow run release-please.yml --ref main -f operation=recover-npm \
  -f candidate_sha="$SHA" -f release_id="$RELEASE_ID" \
  -f release_tag="$TAG" -f source_run_id="$SOURCE_RUN_ID"
```

### Code conventions

- `thiserror` for all public error types in library crates
- `anyhow` only in the binary and engine integration code
- `tracing` for logging, never `println!` in library crates
- No `unwrap()` or `expect()` in library code outside of tests
- Parameterized queries for all LadybugDB operations (no string interpolation)
