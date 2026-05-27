# README Makeover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform the NestWeaver README into a visually engaging, progressive-disclosure layout with animated demo, feature grid, badges, screenshots, and collapsible sections.

**Architecture:** Single-file change to `README.md` plus two visual assets (terminal GIF, web UI screenshot). The README uses only GitHub-supported HTML: `<picture>`, `<details>`, `<table>`, `<p align>`, and shields.io badge images. No external CSS or JavaScript.

**Tech Stack:** GitHub Markdown, HTML subset, shields.io badges, asciinema/VHS for terminal recording (or script-based approach), Chrome browser tools for screenshot capture.

---

### Task 1: Record Terminal Demo GIF

**Files:**
- Create: `assets/demo.gif`

- [ ] **Step 1: Run nestweaver commands and capture output**

Run each command against `testdata/js` to verify output, using the existing release binary:

```bash
cd /Users/korykehl/dev/workspace/nestweaver
rm -f ./nestweaver.lbug  # clean slate
./target/release/nestweaver index --repo ./testdata/js
./target/release/nestweaver context greet
./target/release/nestweaver repo-map --token-budget 200
```

- [ ] **Step 2: Create an SVG terminal recording**

Since VHS and asciinema are not installed, use `script` + a Python SVG generator, OR install asciinema via pip/brew. Alternatively, create a static SVG that simulates terminal output using the actual captured output from Step 1. The SVG approach produces a crisp, small file that renders perfectly on GitHub.

Create `assets/demo.svg` — a dark-background SVG with monospace text showing the three commands and their output, styled to look like a terminal session. Use the actual output captured in Step 1.

- [ ] **Step 3: Verify the asset renders**

Open the SVG in a browser to confirm it looks correct:
```bash
open assets/demo.svg
```

- [ ] **Step 4: Commit**

```bash
git add assets/demo.svg
git commit -m "feat: add terminal demo SVG for README"
```

---

### Task 2: Capture Web UI Screenshot

**Files:**
- Create: `assets/web-ui-screenshot.png`

- [ ] **Step 1: Start the web UI dev server**

The Vite dev server should already be running on a localhost port, or start it:
```bash
cd /Users/korykehl/dev/workspace/nestweaver/crates/nestweaver-web/frontend
npm run dev
```

- [ ] **Step 2: Index testdata and start the backend**

```bash
cd /Users/korykehl/dev/workspace/nestweaver
./target/release/nestweaver index --repo ./testdata/js
./target/release/nestweaver ui --db ./nestweaver.lbug --port 9880
```

- [ ] **Step 3: Take a screenshot via Chrome browser tools**

Navigate to `http://localhost:9880`, wait for the graph to render, and capture a screenshot using `mcp__claude-in-chrome__computer` with `save_to_disk: true`. Save as `assets/web-ui-screenshot.png`.

- [ ] **Step 4: Commit**

```bash
git add assets/web-ui-screenshot.png
git commit -m "feat: add web UI screenshot for README"
```

---

### Task 3: Rewrite README — Hero Section

**Files:**
- Modify: `README.md:1-11`

- [ ] **Step 1: Replace the hero section**

Replace lines 1-11 of `README.md` with:

```html
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-full-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-full-light.svg">
    <img src="assets/logo-full-dark.svg" width="400" alt="NestWeaver">
  </picture>
</p>

<p align="center">
  <strong>Your codebase as a queryable graph — built for AI agents.</strong>
</p>

<p align="center">
  <a href="https://github.com/Kehl-io/nestweaver/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Kehl-io/nestweaver/ci.yml?branch=main&label=CI&style=flat-square" alt="CI"></a>
  <a href="https://github.com/Kehl-io/nestweaver/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="MIT License"></a>
  <a href="https://github.com/Kehl-io/nestweaver/releases"><img src="https://img.shields.io/github/v/release/Kehl-io/nestweaver?style=flat-square&label=release" alt="Latest Release"></a>
  <img src="https://img.shields.io/badge/rust-1.85%2B-orange?style=flat-square" alt="Rust 1.85+">
</p>

<p align="center">
  NestWeaver parses 16 languages, resolves cross-file references with confidence scoring,<br>
  and gives agents precomputed answers about symbols, dependencies, and call graphs —<br>
  no source reading required.
</p>
```

- [ ] **Step 2: Verify badges resolve**

Open `https://img.shields.io/github/actions/workflow/status/Kehl-io/nestweaver/ci.yml?branch=main` in a browser to confirm it returns a valid badge.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add hero section with badges and tagline"
```

---

### Task 4: Rewrite README — Terminal Demo + Feature Grid

**Files:**
- Modify: `README.md` (after hero section)

- [ ] **Step 1: Add terminal demo section**

Insert after the hero, before any other content:

```html
<p align="center">
  <img src="assets/demo.svg" width="700" alt="NestWeaver terminal demo">
</p>

<p align="center"><em>Index a repo and query it in seconds</em></p>
```

- [ ] **Step 2: Add feature highlights grid**

Insert after the demo:

```html
<table>
<tr>
<td width="50%" valign="top">

**16 Languages**<br>
Tree-sitter parsing for JavaScript, TypeScript, Go, Python, Rust, Java, C/C++, and 8 more.

</td>
<td width="50%" valign="top">

**Markdown Brain**<br>
Index Obsidian vaults alongside code. Unified knowledge graph across notes and symbols.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**Task-Focused Context**<br>
Personalized PageRank surfaces only the symbols relevant to your current work.

</td>
<td width="50%" valign="top">

**MCP Server**<br>
17 tools for AI agents via the Model Context Protocol. Drop-in for any MCP client.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**Blast-Radius Analysis**<br>
Trace the impact of a change through the full dependency graph before you ship.

</td>
<td width="50%" valign="top">

**Interactive Web UI**<br>
Graph visualization with force physics, community detection, and semantic zoom.

</td>
</tr>
</table>
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add terminal demo and feature highlights grid"
```

---

### Task 5: Rewrite README — Quick Start + Install

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add Quick Start section**

```markdown
## Quick Start

```sh
# Install (requires Rust 1.85+)
cargo install --path .

# Index a repository
nestweaver index --repo ./my-project

# Get task-focused context for a symbol
nestweaver context processPayment
```

Run `nestweaver --help` for the full command list. All commands support `--json` for machine-readable output.
```

- [ ] **Step 2: Add Install section with collapsible alternatives**

```markdown
## Install

```sh
cargo install --path .
```

<details>
<summary>Pre-built binaries</summary>

Download from [GitHub Releases](https://github.com/Kehl-io/nestweaver/releases) for Linux and macOS (x86_64 and aarch64).

</details>

<details>
<summary>Build from source without installing</summary>

```sh
cargo build --release
# Binary at ./target/release/nestweaver
```

</details>
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add quick start and collapsible install section"
```

---

### Task 6: Rewrite README — CLI Reference (Collapsible Groups)

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add CLI Reference with 4 collapsible groups**

```markdown
## CLI Reference

<details>
<summary><strong>Core Commands</strong></summary>

| Command | Description |
|---------|-------------|
| `index` | Index a local repository into the code graph |
| `context` | Task-focused subgraph via Personalized PageRank |
| `search` | Search symbols by name substring |
| `symbol` | Look up a symbol by name or UID |
| `impact` | Analyze blast radius of a symbol change |
| `repo-map` | Ranked structural skeleton within a token budget |

</details>

<details>
<summary><strong>Brain Commands</strong></summary>

| Command | Description |
|---------|-------------|
| `brain add` | Index a markdown vault into the brain |
| `brain search` | Search notes, headings, and sections |
| `brain context` | Unified PPR context across code + notes |
| `brain list` | List all indexed vaults |
| `brain status` | Show vault count, note count, staleness |
| `brain watch` | Watch a vault for changes and keep in sync |
| `brain refresh` | Force a full or incremental re-index |

</details>

<details>
<summary><strong>Multi-Repo & Projects</strong></summary>

| Command | Description |
|---------|-------------|
| `list-projects` | List all declared and detected projects |
| `project-context` | Project-scoped context (notes + symbols + components) |
| `suggest-links` | Analyze repos and suggest cross-repo links |
| `list-links` | Display declared cross-repo links |
| `list-features` | Display declared feature bundles |
| `clusters` | List detected code communities (Leiden clustering) |
| `cross-repo-refs` | Show cross-repo references for a symbol |

</details>

<details>
<summary><strong>Server & Admin</strong></summary>

| Command | Description |
|---------|-------------|
| `mcp` | Run the MCP server on stdio (17 tools) |
| `ui` | Start the web UI with graph visualization |
| `generate-guide` | Generate an AGENTS.md codebase intelligence guide |
| `embed` | Generate embeddings for all symbols |
| `pull` | Pull repository source on demand |
| `instance` | Manage instances (register, list, remove, pull) |
| `snapshot` | Manage graph snapshots (build, verify, push) |
| `list-repos` | List all indexed repositories |
| `list-services` | List all services/modules |
| `service-summary` | Show a service summary with entry points |

</details>
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add collapsible CLI reference groups"
```

---

### Task 7: Rewrite README — Features (Collapsible), MCP, Web UI

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add collapsible Features section**

```markdown
## Features

<details>
<summary><strong>Markdown Brain</strong></summary>

Index Obsidian vaults and plain markdown folders alongside code, creating a unified knowledge graph that connects notes, headings, sections, and tags to code symbols.

```sh
nestweaver brain add ~/Documents/Obsidian/MyVault
nestweaver brain search "architecture"
nestweaver brain context "MyProject"
```

</details>

<details>
<summary><strong>Projects</strong></summary>

Declare projects that span multiple repos and vaults. Projects aggregate notes, symbols, and components into a single queryable unit.

```toml
# In nestweaver-instance.toml
[[projects]]
name = "my-project"
aliases = ["MP"]
vault_folder = "Projects/my-project"
repos = ["frontend", "backend", "shared"]
```

```sh
nestweaver project-context "my-project" --token-budget 5000
```

</details>

<details>
<summary><strong>Multi-Repo & Instance Config</strong></summary>

Cross-repo links, feature bundles, and project declarations for multi-repo projects. Manifest dependencies detected automatically from package.json, go.mod, Cargo.toml, pyproject.toml, and 8 more formats.

See the [Instance Config Guide](docs/guide/instance-config.md) for setup.

```sh
nestweaver suggest-links --db ./all-repos.lbug
nestweaver context --feature device-pairing --config ./nestweaver-instance.toml
```

</details>
```

- [ ] **Step 2: Add MCP Server section (visible)**

```markdown
## MCP Server

17 tools via the Model Context Protocol for AI agents:

```sh
nestweaver mcp --db ./nestweaver.lbug
```

Tools include: `brain_context`, `brain_search`, `brain_impact`, `brain_status`,
`brain_guide`, `brain_add_source`, `brain_diff`, `note_get`, `backlinks`,
`flow_trace`, `detect_changes`, `clusters`, `stale_check`, `cross_repo_contracts`,
`project_context`, `set_extension`, `query_extensions`.
```

- [ ] **Step 3: Add Web UI section with screenshot**

```markdown
## Web UI

Interactive graph visualization with hover spotlight, community detection, force physics controls, and dark mode:

```sh
nestweaver ui --db ./nestweaver.lbug --port 8080
```

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/web-ui-screenshot.png">
    <img src="assets/web-ui-screenshot.png" width="700" alt="NestWeaver Web UI">
  </picture>
</p>
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: add features, MCP, and web UI sections"
```

---

### Task 8: Rewrite README — Architecture, Contributing, Footer

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add collapsible Architecture section**

```markdown
## Architecture

<details>
<summary>Cargo workspace with 8 internal crates compiling to a single static binary</summary>

| Crate | Purpose |
|-------|---------|
| `nestweaver-schema` | Node/edge types, UIDs, confidence scoring, schema versioning |
| `nestweaver-parser` | Tree-sitter + regex parsing for 16 languages |
| `nestweaver-resolver` | Cross-file symbol resolution with import graphs |
| `nestweaver-store` | LadybugDB graph store, PageRank, hybrid search |
| `nestweaver-storage` | Pluggable snapshot storage backends (local, S3, GitLab) |
| `nestweaver-engine` | Indexing pipeline, query dispatch, config, snapshots, LLM integration |
| `nestweaver-mcp` | Optional MCP server for non-shell AI clients |
| `nestweaver-web` | Optional web UI and API backend |

```
schema          (zero internal deps)
  <- parser
  <- resolver
  <- store
storage         (zero internal deps)
       <- engine <- (parser, resolver, store, storage)
            <- mcp
            <- web
```

</details>
```

- [ ] **Step 2: Add Contributing, License, and Footer**

```markdown
## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for commit conventions, dev commands, and how to submit changes.

## License

MIT

---

<p align="center">
  <a href="https://kehl.io">
    <img src="assets/kehl-io/kehl-icon.png" width="56" alt="kehl.io" />
  </a>
  <br>
  <sub>Built by <a href="https://kehl.io">kehl.io</a></sub>
</p>
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add architecture, contributing, and footer"
```

---

### Task 9: Final Assembly and Review

**Files:**
- Modify: `README.md` (full file — assemble all sections in order)

- [ ] **Step 1: Assemble the complete README**

Combine all sections from Tasks 3-8 into the final `README.md` in this order:
1. Hero (logo, badges, tagline, hook)
2. Terminal demo + caption
3. Feature highlights grid
4. Quick Start
5. Install (with collapsible alternatives)
6. CLI Reference (4 collapsible groups)
7. Features (3 collapsible deep-dives)
8. MCP Server
9. Web UI + screenshot
10. Architecture (collapsible)
11. Contributing + License
12. Footer

- [ ] **Step 2: Verify rendering on GitHub**

Push to a branch and check the rendered README on GitHub, or verify locally with a markdown previewer. Check:
- Badges render with correct status
- `<picture>` logo switches with dark/light mode
- `<details>` sections expand/collapse
- Feature grid table renders without borders
- Demo SVG and screenshot display correctly
- All links work

- [ ] **Step 3: Final commit and push**

```bash
git add README.md assets/
git commit -m "docs: complete README makeover with progressive disclosure layout"
git push
```
