# Installing NestWeaver

Install the CLI from npm or a verified pre-built archive from
[GitHub Releases](https://github.com/Kehl-io/nestweaver/releases/latest), or
build from source. There is currently no published crates.io package.

## npm

Requires Node.js 18+ and one of the supported macOS/Linux platforms below.

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

Building from source requires the current stable Rust toolchain (as used in
CI), CMake, a C++20 compiler, Python 3.9
or newer (LadybugDB's build generates a header with it), OpenSSL and zstd
development files, `pkg-config`, and Protocol Buffers:

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
eval "$(scripts/fetch-lbug-source.sh)"
cargo install --locked --path .
nestweaver --version
```

The first source build compiles LadybugDB and can take several minutes. Keep
the checkout's `.cargo/config.toml`: it forces `LBUG_BUILD_FROM_SOURCE=1` so
the build compiles Ladybug from source, rather than linking a prebuilt archive
whose hidden ELF symbols cause zstd link errors.

**`LBUG_SOURCE_DIR` is required for now.** The published `lbug` 0.21.1 crate
cannot build its own bundled sources (the packaging fix is merged upstream but
not yet released), so the build reads them from a LadybugDB checkout at tag
`v0.21.1` named by `LBUG_SOURCE_DIR`. `scripts/fetch-lbug-source.sh` makes a
shallow clone of that tag (about 80 MB, no submodules) under `target/`,
verifies its commit, and prints the `export` line; `eval` applies it to the
current shell. Pass a directory to put the clone elsewhere, or point
`LBUG_SOURCE_DIR` at your own checkout of the same tag. Set it in every shell
that runs `cargo`. This step goes away once a published `lbug` crate builds
from source.

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

## Upgrading an existing database

**Storage compatibility comes first.** A database built before LadybugDB 0.21
is refused with `nestweaver::db_rebuild_required`. Back it up and follow the
[publication rebuild guide](docs/guide/publication-rebuild.md#storage-engine-upgrade-ladybugdb-021-rebuild-required)
and the error's recovery command. A forced re-index alone is not the remedy
for an incompatible storage format.

For a compatible database, the current **`RESOLVER_GENERATION` is 9**.
Compatibility requires an exact match: a graph written by an older or newer
resolver must be fully re-indexed before you trust its edges and rankings.

```sh
nestweaver stale-check --db /path/to/nestweaver.lbug
nestweaver index --repo /path/to/repo --db /path/to/nestweaver.lbug --force
```

`stale-check` reports `outdated_resolver` and exits 2 for a resolver mismatch.
**`--force` is required** for each repository: an incremental index at git
HEAD can report no changes and leave the old generation in place. Vaults are
not repositories and carry no code resolver generation.

See the [README upgrade notes](README.md#upgrading--re-index-before-you-trust-a-ranking)
for the resolver history, Markdown derivation changes, and script/HTTP-client
migration details.

## Configure for your AI tool

```sh
nestweaver setup --db "$(pwd)/nestweaver.lbug"
```

## Index and verify

```sh
nestweaver index --repo . --db "$(pwd)/nestweaver.lbug"
nestweaver search "main" --db ./nestweaver.lbug
```

Index prints its selected database target on success. From another directory,
use that same absolute path with `--db` for search and MCP.

## Start the MCP server

```sh
nestweaver mcp --db "$(pwd)/nestweaver.lbug"
```

The daemon owns the database exclusively. It auto-starts on first use and logs
to `~/.local/state/nestweaver/<instance>/daemon.log`.

Daemon bypass is CI-only and is disabled in all standard development and
release artifacts, including when a runtime permit is set. Local CLI, MCP,
HTTP, fixtures, and recovery use a daemon. An unavailable daemon must be
reconnected or diagnosed; bypassing it is not a recovery option.

Internal CI direct-store suites use a separately built, unpublished
`ci-direct-tests` artifact, an explicit `NESTWEAVER_ALLOW_NO_DAEMON=1` permit,
and a GitHub Actions runner context. Ambient `CI=true` is not a permit. CI must
also test the standard artifact through a real isolated daemon with bypass
variables absent.

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
