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

Current macOS release archives are **CPU-only**: they are not built with the
Metal feature. This warning must be removed only in the release that passes the
artifact capability smoke test from Metal Task 5.

## Build from source

Building from source requires Rust 1.85+.

```sh
git clone https://github.com/Kehl-io/nestweaver.git
cd nestweaver
cargo install --locked --path .
nestweaver --version
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
