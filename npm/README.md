# nestweaver

Code knowledge graph for AI agents — 42 MCP tools, 32 languages, graph
visualization.

This package is a thin wrapper with **no install-time (`postinstall`) script**.
It ships one prebuilt-binary package per platform —
`nestweaver-darwin-arm64`, `nestweaver-darwin-x64`, `nestweaver-linux-arm64`,
`nestweaver-linux-x64` — as an `optionalDependencies` entry, the same pattern
esbuild, swc, and Rollup use. npm, pnpm, and Yarn each install only the one
matching your machine's `os`/`cpu` through their own ordinary dependency
resolution; the `nestweaver` executable on your PATH resolves and runs
whichever one actually landed, at invocation time.

**No lifecycle scripts to allow.** There is nothing for
`npm install --ignore-scripts` to skip, and pnpm 10+'s default block on
lifecycle scripts (which used to leave a wrapper with no binary and no
`pnpm.onlyBuiltDependencies` clue why) no longer applies — there is no script
to block.

If your package manager skipped optional dependencies entirely
(`--omit=optional`, `--no-optional`, or a lockfile resolved without one),
`nestweaver` exits 1 and names the exact optional package to install instead
of silently doing nothing. If your organisation disallows this platform
package entirely, install a release archive from GitHub or build from source
(`cargo install --locked --path .`) instead.

## Platforms

macOS and Linux, on x86_64 and arm64. Every other platform has no published
optional package, so `nestweaver` FAILS with the supported-targets list at
invocation time rather than leaving you with a wrapper that silently cannot
run; use a release archive or a source build instead. The two Linux platform
packages additionally declare `libc: ["glibc"]` (musl/Alpine is not
currently supported) — this is safe to declare on a Linux-only package in a
way it never was on the combined macOS+Linux wrapper (see nw-433 below).

Linux builds target **glibc 2.35**, which covers Ubuntu 22.04 LTS and newer and
Debian 12. The platform package includes the GCC 13 runtime LadybugDB needs
beside the binary. Check your glibc with `ldd --version`; on anything older
the binary will not start and the error names a missing `GLIBC_` symbol.
macOS builds target **13.3**.

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
