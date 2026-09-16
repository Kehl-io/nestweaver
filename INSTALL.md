# Installing NestWeaver

Install the CLI from npm or a verified pre-built archive from
[GitHub Releases](https://github.com/Kehl-io/nestweaver/releases/latest), or
build from source. There is currently no published crates.io package.

## npm

```sh
npm install --global nestweaver
nestweaver --version
```

The `nestweaver` package has no install-time (`postinstall`) script. It ships
one prebuilt-binary package per platform (`nestweaver-darwin-arm64`,
`nestweaver-darwin-x64`, `nestweaver-linux-arm64`, `nestweaver-linux-x64`) as
an `optionalDependencies` entry -- the same pattern esbuild, swc, and Rollup
use. npm, pnpm, and Yarn install only the one matching your machine's `os`
and `cpu` through their own normal dependency resolution; the `nestweaver`
executable resolves and runs whichever one landed at invocation time. This
means:

- **No lifecycle scripts to allow.** pnpm 10+ blocks `postinstall` by
  default; there is no longer one to block, and no
  `pnpm.onlyBuiltDependencies` configuration is needed.
- **Nothing is downloaded from GitHub at install time.** The binary is part
  of the platform package's own npm tarball, verified by npm's standard
  package integrity check like any other dependency.
- An unsupported platform, or a lockfile/flag that skipped optional
  dependencies (`--omit=optional`, `--no-optional`), produces a clear error
  naming the exact optional package to install, rather than a wrapper that
  silently has no binary.

See the platform baselines below when installing on Linux or macOS.

## Pre-built CLI (recommended)

Open the latest GitHub Release and download the archive and matching `.sha256`
file for your platform. Release archive names follow this pattern:

| Platform | Release target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

### Platform baselines

These are the floors the release archives are built against, and they are
enforced in CI against the artifact itself rather than assumed from the build
host.

| Platform | Baseline | Covers |
| --- | --- | --- |
| Linux (both architectures) | **glibc 2.35** | Ubuntu 22.04 LTS and newer, Debian 12 |
| macOS | **13.3** | LadybugDB uses floating-point `std::format` |

Check your glibc with `ldd --version`. If it is older than 2.35 — including
RHEL/Rocky 9, RHEL 8, Ubuntu 20.04, and Debian 11 — the GNU archives will not
start, and the failure is a loader error naming a missing `GLIBC_` symbol
rather than anything from NestWeaver. Build from source on those systems.

Releases through v8.0.0 were built on `ubuntu-latest`, which moved to 24.04 and
raised the shipped floor to glibc 2.39 without anyone declaring it, so those
archives do not start on any of the distributions listed above. If you are on
v8.0.0 and hit a `GLIBC_` loader error, that is this defect; upgrade to 9.0.0
or later.

For the selected release tag and target, download
`nestweaver-<tag>-<target>.tar.gz` and
`nestweaver-<tag>-<target>.tar.gz.sha256`. Do not substitute a version number
in this guide; use the tag displayed by the release you selected.

Verify the archive before extracting it. Linux archives also contain a `lib/`
directory with the exact GCC 13 C++ runtime used by LadybugDB; keep that
directory beside the executable. Installing only the Linux `nestweaver` file
will leave its required runtime behind.

```sh
ARCHIVE="nestweaver-<tag>-<target>.tar.gz"
shasum -a 256 -c "$ARCHIVE.sha256"
INSTALL_DIR="/opt/nestweaver-<tag>"
sudo mkdir -p "$INSTALL_DIR"
sudo tar -xzf "$ARCHIVE" -C "$INSTALL_DIR"
sudo ln -sfn "$INSTALL_DIR/nestweaver" /usr/local/bin/nestweaver
nestweaver --version
```

On Linux, `sha256sum -c "$ARCHIVE.sha256"` is an equivalent checksum command.

Starting with the release that contains this change, both macOS CLI archives
are built with Metal support. To opt out explicitly and use CPU embeddings,
set:

```toml
[embedding]
accelerator = "cpu"
```

## Build from source

Building from source requires Rust 1.85+, CMake, a C++20 compiler, OpenSSL and
zstd development files, `pkg-config`, and Protocol Buffers:

```sh
# macOS with Homebrew (also install the Xcode Command Line Tools)
brew install cmake openssl@3 pkg-config protobuf zstd

# Debian/Ubuntu
sudo apt-get update
sudo apt-get install -y cmake g++ libssl-dev libzstd-dev pkg-config protobuf-compiler
```

```sh
git clone https://github.com/Kehl-io/nestweaver.git
cd nestweaver
cargo install --locked --path .
nestweaver --version
```

The first source build compiles LadybugDB and can take several minutes. Keep
the checkout's `.cargo/config.toml`: it forces `LBUG_BUILD_FROM_SOURCE=1` so
the build uses the Ladybug sources resolved by `Cargo.lock`, rather than a
prebuilt archive whose hidden ELF symbols cause zstd link errors. Cargo
vendors the Ladybug sources with the crate.

No extra linker flags are needed. Exactly one copy of zstd is linked: the one
Ladybug vendors. Rust code uses it through `nestweaver_store::zstd` instead of
the `zstd` crate, which would compile a second copy and break the link on
`x86_64-unknown-linux-gnu`.

If OpenSSL is installed in a non-default location and the linker cannot find
`ssl` or `crypto`, expose that installation while building:

```sh
# macOS with Homebrew
LIBRARY_PATH="$(brew --prefix openssl@3)/lib" cargo install --locked --path .
```

Semantic embeddings are included by default. On macOS, build with Metal when
you want GPU-accelerated embeddings:

```sh
cargo install --locked --path . --features metal
```

There is no feature matrix table here because there is only one feature split
that changes user-visible behaviour, and it is this: **the nw-484 background
auto-repair of a missing model cache exists only in builds with the `embed`
feature.** It is on by default there (`[embedding] auto_repair_cache = true`,
opt-out), and it re-fetches only the model this database's own verified
identity already names — never a first-time download. A build without `embed`
— including `--no-default-features --features metal` — has no local cache to
repair and the key is not present at all, so a missing cache stays an
operator-initiated `nestweaver embed` away. The released archives and the npm
platform packages are `embed` builds; only a hand-configured source build can
land on the other side of this.

## macOS app

The native `NestWeaver.app` is source-build-only until a release job publishes
a `.app` bundle or DMG. Build it from a checkout with `app/build.sh`; that build
uses the Metal feature.

```sh
bash app/build.sh
open target/release/NestWeaver.app
```

## Upgrading to this release — re-index every graph

**This release bumps `RESOLVER_GENERATION` to 6.** Installing the new binary
does not repair a graph already on disk. Until each repo is re-indexed, its
rankings, edges and `dead-code` results are computed from data the old resolver
wrote, and `dead-code` refuses outright rather than reporting from it.

What generation 6 changes is which symbols are ENTRY POINTS, and
`is_entry_point`/`entry_point_kind` are persisted per-symbol columns read
straight off disk rather than re-derived at query time — so a symbol indexed
before this release keeps `is_entry_point: false` forever, no matter which
binary asks. It covers bash and Python top-level-call rooting, Swift
`main.swift`/shebang rooting, bash `trap` handlers, `package.json` entry points
discovered at any depth in a monorepo, and a C++ span-anchoring and
struct-typed-local fix that changes symbol UIDs (and therefore edge endpoints)
in error-recovered, macro-prefixed declarations.

```sh
nestweaver index --repo <path> --force   # per repo — a full index, not incremental
```

**`--force` is required, not a suggestion.** A generation-stale repo is at HEAD
with nothing modified, so a plain `nestweaver index --repo <path>` takes the
incremental path, reports `0 modified`, writes nothing, and leaves both the old
data and the old generation in place. Vaults are not repos, carry no resolver
generation, and do not need refreshing for this.

## Upgrading from 8.x — re-index every graph

**9.0.0 bumps `RESOLVER_GENERATION` from 3 to 4.** Installing the new binary
does not repair edges already on disk. Until each repo is re-indexed, its
rankings, C/C++ `MEMBER_OF` edges and C++ `IMPORTS` edges are wrong, and `.h`
files carry symbols extracted by the C grammar rather than C++.

```sh
nestweaver index --repo <path> --force   # per repo — a full index, not incremental
```

**`--force` is not optional.** A generation-stale repo is at HEAD with every
file unchanged, so plain `nestweaver index --repo <path>` reports `0 modified`,
writes nothing, and leaves the old edges — and the old generation — in place.
Vaults carry no resolver generation and do not need refreshing for this.

**`nestweaver stale-check` detects it as of 9.0.0** (in 8.x it did not — its
ladder was SHA-vs-HEAD only, so a generation-3 graph exited 0):

```sh
nestweaver stale-check                 # status: outdated_resolver, exit 2
nestweaver stale-check --json | jq '{any_needs_reindex, resolver_stale_repos}'
# or read the sidecar directly — any repo whose generation DIFFERS from the
# running binary's `RESOLVER_GENERATION` (6 in this release) needs a re-index.
# As of 9.1.0 compatibility is an exact match, not a floor: a graph written by
# a NEWER resolver than the running binary understands is just as untrustworthy
# as an older one, so it reads as stale too.
cat <db>.resolver_generation.json
```

`hubs`, `bridges`, `repo-map`, `ranking rank` and `summary --level hub` also
disclose it. `clusters`, `blast-radius`, `generate-guide`, PPR-backed `context`
and the web UI still do not — on those, treat the absence of a warning as no
evidence either way.

## Configure for your AI tool

```sh
nestweaver setup
```

## Index and verify

```sh
nestweaver index --repo .
nestweaver search "main"
```

## Start the MCP server

```sh
nestweaver mcp --db ./nestweaver.lbug
```

The daemon owns the database exclusively. It auto-starts on first use and logs
to `~/.local/state/nestweaver/<instance>/daemon.log`.

In CI, `--no-daemon` / `NESTWEAVER_NO_DAEMON=1` only **request** a daemon
bypass. `NESTWEAVER_ALLOW_NO_DAEMON=1` is the only thing that **permits** one —
`CI=true` and `GITHUB_ACTIONS` confer nothing. Without the opt-in the flag is
disclosed on stderr and the command autostarts a daemon anyway, which then holds
the write lease for the rest of the job.

## Optional: Git history analysis

```sh
nestweaver index --repo . --with-git-activity
```

## Server mode

NestWeaver can connect to a shared upstream server for org-wide code
intelligence:

```sh
nestweaver connect <url> --token <bearer-token>
```

Or set the environment variable:

```sh
export NESTWEAVER_UPSTREAM=grpcs://nestweaver.example.com:9378
```

See [Server Mode](docs/server-mode.md) for routing behavior and configuration.
