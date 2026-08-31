# Installing NestWeaver

Install a pre-built CLI from [GitHub Releases](https://github.com/Kehl-io/nestweaver/releases/latest), or build from source. There is currently no published npm or crates.io package.

## Pre-built CLI (recommended)

Open the latest GitHub Release and download the archive and matching `.sha256`
file for your platform. Release archive names follow this pattern:

| Platform | Release target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

For the selected release tag and target, download
`nestweaver-<tag>-<target>.tar.gz` and
`nestweaver-<tag>-<target>.tar.gz.sha256`. Do not substitute a version number
in this guide; use the tag displayed by the release you selected.

Verify the archive before extracting it:

```sh
ARCHIVE="nestweaver-<tag>-<target>.tar.gz"
shasum -a 256 -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE"
sudo install -m 755 nestweaver /usr/local/bin/nestweaver
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

## macOS app

The native `NestWeaver.app` is source-build-only until a release job publishes
a `.app` bundle or DMG. Build it from a checkout with `app/build.sh`; that build
uses the Metal feature.

```sh
bash app/build.sh
open target/release/NestWeaver.app
```

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
# or read the sidecar directly — any repo below 4 needs a re-index:
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
