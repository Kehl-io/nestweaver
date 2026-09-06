# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in NestWeaver, please report it
responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please email **kory@kehl.io** with:

- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested fixes (optional)

You should receive a response within 72 hours. Once the issue is confirmed,
a fix will be developed and released as a patch version before public
disclosure.

## Scope

Security issues include but are not limited to:

- Cypher/query injection via crafted symbol names or file paths
- Path traversal allowing file access outside the workspace sandbox
- Command injection via git operations
- Snapshot tampering bypassing integrity checks
- Credential leakage through config files or logs

## Supported Versions

Only the latest published release is supported. Please upgrade before
reporting an issue — it may already be fixed.

## Threat model & known residuals

Server mode is designed for **self-hosted, trusted-network** deployment: the org
query token grants read access to the whole indexed graph, and the admin token
grants full control. It is not multi-tenant. Within that model, the following are
known, accepted residual risks (not treated as vulnerabilities):

- **SSH clone DNS-rebinding TOCTOU.** `http(s)` clone/fetch pins the resolved IP
  (`http.curloptResolve` + `followRedirects=false`); `ssh://` cannot be pinned via
  the git CLI, so an admin-added `ssh://` repo whose hostname re-resolves to an
  internal IP between validation and connect could reach an internal SSH host.
  Requires admin access + attacker DNS control. Prefer `https` remotes for
  untrusted URLs.
- **Backup archive expansion.** `nestweaver backup restore` / `inspect` decompress
  an operator-supplied `.nwsnap.zst` without an uncompressed-size cap; a crafted
  archive could exhaust memory/disk. Restore is a local admin/CLI operation on a
  trusted file — do not restore snapshots from untrusted sources. (Path traversal
  / zip-slip is already prevented by the `tar` crate's `unpack`.)
- **`git rev-parse <ref>`** in the bare-clone reader passes a config/HEAD-derived
  ref without an end-of-options guard. Not attacker-reachable today (refs are
  `refs/heads/<branch>` / `HEAD` / `FETCH_HEAD`), noted for defense-in-depth.
- **Advisory-flagged transitive dependencies.** `cargo audit` reports no
  *vulnerability* advisories and exits 0. It does report three warnings, all in
  transitive dependencies, verified on 2026-09-05:
  - `paste` 1.0.15 — unmaintained ([RUSTSEC-2024-0436]).
  - `cxx` 1.0.138 — unsound: `let_cxx_string!` can expose an uninitialized
    value under an exception-safety violation ([RUSTSEC-2026-0202]). Reached
    only through the storage engine's C++ bridge; NestWeaver does not use that
    macro directly.
  - `lru` 0.16.4 — unsound: potential use-after-free if a user-supplied `Drop`
    panics inside `LruCache::pop()` ([RUSTSEC-2026-0253]). Reached via
    `tantivy` 0.26.1 only. `nestweaver-store`'s own direct `lru` dependency is
    0.18.2, which is NOT affected — both versions are in the tree, so checking
    the direct dependency alone would give a misleading all-clear.

  Tracked, no known exploit path. Two crates previously named here
  (`number_prefix`, `rustls-pemfile`) have since dropped out of the tree
  entirely. Re-run `cargo audit` before each release — this list went stale
  once already, and "no exploitable advisories" is not the same claim as
  "no advisories".

[RUSTSEC-2024-0436]: https://rustsec.org/advisories/RUSTSEC-2024-0436
[RUSTSEC-2026-0202]: https://rustsec.org/advisories/RUSTSEC-2026-0202
[RUSTSEC-2026-0253]: https://rustsec.org/advisories/RUSTSEC-2026-0253
