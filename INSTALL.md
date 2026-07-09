# Installing NestWeaver

Step-by-step guide for installing and configuring NestWeaver.

## Option 1: npm (recommended, no Rust needed)

```bash
npm install -g @kehl-io/nestweaver
nestweaver --version
# Expected: nestweaver X.Y.Z
```

## Option 2: Cargo (Rust users)

Build and install from a local checkout (crates.io publishing is not yet
automated, so `cargo install nestweaver` from the registry may lag the latest
release — build from source to guarantee the current version):

```bash
git clone https://github.com/Kehl-io/nestweaver && cd nestweaver
cargo install --path .
nestweaver --version
# Expected: nestweaver X.Y.Z
```

Semantic embeddings are included by default (`embed` feature). On **macOS**, add
`--features metal` for GPU-accelerated embeddings from the CLI:

```bash
cargo install --path . --features metal   # macOS: Metal GPU embeddings
```

> The macOS `.app` bundle (below) is already built with Metal and is the
> recommended way to run on a Mac.

> **Building from source?** If you previously installed via `cargo install` and
> are now building from a local checkout, run `cargo install --path .` after
> each build to update `~/.cargo/bin/nestweaver`. Otherwise the installed binary
> will be stale and may lack newer subcommands (e.g. `server`, `connect`).

## Option 3: Pre-built binary

Download the latest release for your platform from
[GitHub Releases](https://github.com/Kehl-io/nestweaver/releases/latest).

Extract and install:
```bash
tar xzf nestweaver-*.tar.gz
sudo mv nestweaver /usr/local/bin/
```

## Configure for your AI tool

```bash
nestweaver setup
# Expected output:
# NestWeaver Setup
# ────────────────────────────────────────
# ✓ Claude Code — .claude/settings.json — MCP server configured
# ✓ Cursor — .cursor/mcp.json — MCP server (lite: 6 tools)
# ...
```

## Index your codebase

```bash
nestweaver index --repo .
# Expected: Indexing /path/to/repo → ./nestweaver.lbug
```

## Add a markdown vault (optional)

```bash
nestweaver brain add ~/path/to/vault
# Expected: Indexed vault '...': N note(s), M heading(s), ...
```

## Verify

```bash
nestweaver search "main"
# Expected: Found N symbol(s) matching 'main':

nestweaver brain status
# Expected: Brain status with vault counts
```

## Start the MCP server

```bash
nestweaver mcp --db ./nestweaver.lbug
# Auto-starts a background daemon on first use (~1s startup delay)
# The server runs on stdio, proxying queries through the daemon
```

The daemon owns the database exclusively, enabling concurrent access from multiple AI tools and CLI commands. Daemon logs are at `~/.local/state/nestweaver/<instance>/daemon.log`.

## Optional: Git history analysis

```bash
nestweaver index --repo . --with-git-activity
```

This enables co-change mining (finds files that always change together) and git recency scoring for ranking. Results are stored as sidecars alongside the database.

## Server Mode

NestWeaver can connect to a shared upstream server for org-wide code intelligence.

Connect to a server:
```bash
nestweaver connect <url> --token <bearer-token>
```

Or set the environment variable:
```bash
export NESTWEAVER_UPSTREAM=grpcs://nestweaver.example.com:9378
```

Local queries are automatically augmented with server-side results when an upstream is configured. See `AGENTS.md` for detailed routing behavior and configuration.
