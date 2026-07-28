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
the build uses the reviewed source pinned by `Cargo.lock`, rather than an
unrelated prebuilt archive. Cargo fetches the pinned Ladybug submodule
automatically.

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

### Temporary Ladybug source pin

This revision temporarily patches `lbug` 0.18.2 to wrapper commit
`8992183ff8de526e8a852be8a97ad04b412f56ed`, whose `lbug-src` submodule is
pinned to Ladybug commit
`9e221866e08371d380c8bd91f7bc98d101ebf723`. That commit backports the filtered
multi-segment string-scan correction proposed in
[LadybugDB/ladybug#737](https://github.com/LadybugDB/ladybug/pull/737).

Do not remove the workspace `[patch.crates-io]` entry, delete
`.cargo/config.toml`, or point `LBUG_LIBRARY_DIR`/`LBUG_INCLUDE_DIR` at a
different Ladybug build when validating this fix. The temporary patch can be
removed after upstream publishes an `lbug` release containing the correction
and NestWeaver's storage regression suite passes against that release.

Until a NestWeaver release containing this change is published, build this
revision from source when you specifically need the filtered-scan correction.

## macOS app

The native `NestWeaver.app` is source-build-only until a release job publishes
a `.app` bundle or DMG. Build it from a checkout with `app/build.sh`; that build
uses the Metal feature.

```sh
bash app/build.sh
open target/release/NestWeaver.app
```

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
