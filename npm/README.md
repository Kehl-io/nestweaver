# nestweaver

Code knowledge graph for AI agents — 42 MCP tools, 32 languages, graph
visualization.

This package is a thin wrapper. Its `postinstall` downloads the NestWeaver
binary matching this package's version from the project's GitHub Releases,
verifies it against the SHA-256 published alongside the archive, and puts a
`nestweaver` executable on your PATH.

**Lifecycle scripts must be enabled.** The binary is fetched in `postinstall`,
so `npm install --ignore-scripts` — and **pnpm 10+, which blocks lifecycle
scripts by default** — leave you with a wrapper and no binary. `nestweaver` then
exits 1 saying the binary was not found. With pnpm, allow it explicitly:

```jsonc
// package.json
{ "pnpm": { "onlyBuiltDependencies": ["nestweaver"] } }
```

If your organisation disallows install scripts outright, install a release
archive from GitHub or build from source (`cargo install --locked --path .`)
instead.

## Platforms

macOS and Linux, on x86_64 and arm64. On any other platform the install step
FAILS and prints the supported targets, rather than leaving you with a wrapper
that cannot run; use a release archive or a source build instead.

Linux builds target **glibc 2.35**, which covers Ubuntu 22.04 LTS and newer and
Debian 12. The archive includes the GCC 13 runtime LadybugDB needs and the npm
installer preserves it beside the binary. Check your glibc with `ldd --version`;
on anything older the binary will not start and the error names a missing
`GLIBC_` symbol. macOS builds target **13.3**.

## Upgrading an existing graph

NestWeaver 9.0.0 raised the resolver generation, so a graph built by an earlier
release has stale rankings and is missing C/C++ `MEMBER_OF` and C++ `IMPORTS`
edges. Check with:

```sh
nestweaver stale-check
```

It exits `2` and reports `outdated_resolver` when a re-index is needed. The
remedy needs `--force`, because a generation-stale repo is at HEAD with nothing
modified and a plain re-index takes the incremental path and writes nothing:

```sh
nestweaver index --repo <path> --force
```

## Other installation paths

Verified GitHub Release archives, and building from a source checkout with Rust
1.85+, are documented in [INSTALL.md](https://github.com/Kehl-io/nestweaver/blob/main/INSTALL.md).

## Links

- [Repository and documentation](https://github.com/Kehl-io/nestweaver)
- [Issue tracker](https://github.com/Kehl-io/nestweaver/issues)
- MIT licensed
