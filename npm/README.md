<p align="center">
  <img src="https://raw.githubusercontent.com/Kehl-io/nestweaver/main/npm/media/logo.svg" width="360" alt="NestWeaver">
</p>

<p align="center">
  <strong>Your codebase as a queryable graph, built for AI agents.</strong>
</p>

<p align="center">
  <a href="https://docs.nestweaver.kehl.io">Docs</a> ·
  <a href="https://nestweaver.kehl.io">Website</a> ·
  <a href="https://github.com/Kehl-io/nestweaver">Source</a>
</p>

NestWeaver indexes a repository into a structural graph: symbols, calls, imports, and notes. Agents query that graph instead of reading the tree file by file. This package installs the CLI. It does not download anything at install time.

<p align="center">
  <img src="https://raw.githubusercontent.com/Kehl-io/nestweaver/main/npm/media/demo.svg" width="700" alt="Terminal: index a repo, then ask for context around a symbol">
</p>

## Install and query

```sh
npm install --global nestweaver

nestweaver index --repo .
nestweaver search "main"
nestweaver context processPayment
nestweaver setup
```

`index` takes `--repo`. `search` matches symbol names. `context` returns the symbols around a name, a UID, or a repo-relative file path. `setup` writes the MCP config for the agent it detects. Cursor gets six tools (`--lite`); other agents get the full set of 43.

The database defaults to `./nestweaver.lbug` in the current directory. Pass `--db` when you query from somewhere else.

## See the graph

`nestweaver ui` opens a local workspace on port 3000: an overview of what is indexed, then the source and callers for the symbol you pick.

<p align="center">
  <img src="https://raw.githubusercontent.com/Kehl-io/nestweaver/main/npm/media/web-ui.png" width="720" alt="NestWeaver web UI showing the sample-graph overview">
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/Kehl-io/nestweaver/main/npm/media/web-ui-symbol.png" width="720" alt="NestWeaver web UI with run_query selected, its source, and its callees">
</p>

## What agents get

- **43 MCP tools** for context, impact, tests, and vault notes. The six-tool lite set is `brain_context`, `brain_search`, `brain_impact`, `brain_status`, `brain_guide`, and `detect_changes`.
- **32 languages**, parsed with Tree-sitter, including JavaScript, TypeScript, Python, Go, Rust, Java, C, and C++.
- **Impact and review.** `pr-impact` scores a diff. `affected-tests` lists tests to run. `dead-code` is a review list, not a deletion list.
- **Markdown vaults** indexed next to the code, so notes and symbols share one graph.

Full command and tool reference: [docs.nestweaver.kehl.io](https://docs.nestweaver.kehl.io).

## How this package installs

There is no `postinstall` script. The package depends on one platform binary (`nestweaver-darwin-arm64`, `nestweaver-darwin-x64`, `nestweaver-linux-arm64`, or `nestweaver-linux-x64`). npm, pnpm, and Yarn install the one that matches your machine.

macOS and Linux, x86_64 and arm64. Linux builds need glibc 2.35 (Ubuntu 22.04 and Debian 12, or newer). macOS builds need 13.3. There is no Windows build, no musl/Alpine build, no crates.io package, and no Homebrew formula.

If optional dependencies were skipped, `nestweaver` exits 1 and names the platform package to install. A verified GitHub Release archive, or a source build, is documented in [INSTALL.md](https://github.com/Kehl-io/nestweaver/blob/main/INSTALL.md).

## After you upgrade

The current resolver generation is **9**. A graph recorded at any other generation has stale edges. Check, then re-index with `--force`. A plain `index` does nothing when the repo is already at HEAD.

```sh
nestweaver stale-check
nestweaver index --repo . --force
```

`stale-check` exits 2 and reports `outdated_resolver` when a re-index is required.

## License

MIT. Issues and source: [github.com/Kehl-io/nestweaver](https://github.com/Kehl-io/nestweaver).
