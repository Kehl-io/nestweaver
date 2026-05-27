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
# The server runs on stdio, ready for AI tool connections
```
