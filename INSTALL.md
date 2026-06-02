# Installing NestWeaver

Step-by-step guide for installing and configuring NestWeaver.

## Option 1: npm (recommended, no Rust needed)

```bash
npm install -g @kehl-io/nestweaver
nestweaver --version
# Expected: nestweaver 0.1.0
```

## Option 2: Cargo (Rust users)

```bash
cargo install nestweaver
nestweaver --version
# Expected: nestweaver 0.1.0
```

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
