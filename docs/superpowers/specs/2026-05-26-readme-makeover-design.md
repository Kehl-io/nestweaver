# README Makeover Design Spec

**Date:** 2026-05-26
**Status:** Approved

## Goal

Modernize the NestWeaver README from a plain wall-of-text into a visually engaging, scannable, progressive-disclosure layout that conveys the value prop in 5 seconds and lets readers drill into details on demand.

## Audience

Both AI/LLM tool builders and individual developers, equally weighted.

## Tone

Technical but approachable — confident, direct, shows personality without being cute. Like ripgrep or Deno.

## Structure

### 1. Hero

- Logo: `logo-full-dark.svg` / `logo-full-light.svg` via `<picture>`, 400px (already in place)
- Badges row (centered): CI status, License MIT, Latest Release, Rust 1.85+
- Tagline: "Your codebase as a queryable graph — built for AI agents."
- Hook: one sentence explaining what it does and why it matters

### 2. Terminal Demo

- Animated GIF recorded against `testdata/js` with VHS or asciinema
- Commands: `nestweaver index`, `nestweaver context`, `nestweaver repo-map`
- Centered, auto-playing, dark terminal background
- Caption: "Index a repo and query it in seconds"

### 3. Feature Highlights Grid

- HTML table, 2 columns x 3 rows, no visible borders
- Bold title + one-liner description per cell
- Features: 16 Languages, Markdown Brain, Task-Focused Context, MCP Server, Blast-Radius Analysis, Web UI

### 4. Quick Start

- 3 commands only: install, index, context
- No example output (GIF covers that)
- Single line pointing to `--help` for the full command list

### 5. Install

- Primary: `cargo install --path .` (expanded)
- Pre-built binaries: `<details>` collapsed
- Build from source: `<details>` collapsed

### 6. CLI Reference

- 4 collapsible `<details>` groups:
  - Core Commands (index, context, search, symbol, impact, repo-map)
  - Brain Commands (add, search, context, watch, refresh, status, list)
  - Multi-Repo & Projects (suggest-links, list-links, list-features, project-context, clusters, list-projects)
  - Server & Admin (mcp, ui, pull, instance, snapshot, generate-guide, embed)
- Each group has a table inside the collapsible

### 7. Features

- Single "Features" heading
- One `<details>` per feature: Markdown Brain, Projects, Multi-Repo & Instance Config
- Each contains a short description + code snippet

### 8. MCP Server

- Visible (not collapsed) — key differentiator
- Short description + command + tool list

### 9. Web UI

- Visible (not collapsed) — key differentiator
- Short description + command
- Screenshot of the graph visualization

### 10. Architecture

- Collapsible `<details>` with crate table
- Dependency flow diagram stays visible inside the collapsible

### 11. Contributing + License

- Short, same as current
- Link to CONTRIBUTING.md

### 12. Footer

- kehl.io icon (56px) + "Built by kehl.io" stacked layout (unchanged)

## Visual Assets to Create

1. **Terminal demo GIF** — Record `index`, `context`, `repo-map` against testdata/js
2. **Web UI screenshot** — Capture the graph visualization in dark mode

## GitHub Markdown Constraints

- No `<style>`, `class`, or inline CSS — GitHub strips them
- `<picture>` + `<source>` for dark/light images
- `<details>` / `<summary>` for collapsible sections
- `align="center"` on `<p>` for centering (deprecated HTML but GitHub supports it)
- HTML tables for grid layouts (no borders via empty header trick or minimal styling)
- Badges via shields.io

## Out of Scope

- Terminal demo tool installation (VHS/asciinema) — use whichever is available
- Docs site or separate CLI reference page
- README translations
