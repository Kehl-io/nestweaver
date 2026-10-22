# Threat model: NestWeaver

## What this project does and where untrusted input enters

NestWeaver builds a knowledge graph over source repositories and Markdown vaults (Obsidian-style notes) and serves it
to people and AI agents. One Rust binary, `nestweaver`, is the CLI, a background daemon that owns the LadybugDB graph
store (C++, linked through the `lbug` crate), an MCP server (stdio and HTTP), a gRPC server, and a local web UI.
`.github/SECURITY.md` is the project's security policy; its "Threat model & known residuals" section applies here too.

Treat the following as attacker-controlled:

- **The content of an indexed repository or vault.** Users index code and notes they did not write, including clones
  of third-party repositories. That covers every file the walker reaches, file and directory names, symlinks,
  `.gitignore` files, source parsed by the tree-sitter grammars and the custom parsers in `crates/nestweaver-parser`,
  Markdown, YAML frontmatter, wikilinks, Canvas, Mermaid and Dataview blocks, and git metadata.
- **MCP tool arguments**, over stdio (`nestweaver mcp`) and over HTTP (`POST /mcp`). The caller is an AI agent that
  may itself be steered by prompt injection from indexed content, so a crafted argument is in scope even on loopback.
  That includes symbol names, note titles, paths, regex patterns (`regex_search`), filters and limits.
- **Network peers in server mode** (`nestweaver daemon run --server`): gRPC on `--bind` (default `127.0.0.1:9378`),
  and on port+1 (default 9379) MCP-over-HTTP, `/webhook`, `/admin/api/*`, `/auth/*` (device flow) and `/metrics`.
  Unauthenticated peers reach the request parsing, the auth checks and the webhook signature checks.
- **Webhook payloads and remote repository URLs**, which make the daemon clone or fetch: SSRF, git option and
  argument injection, and hostile git servers.
- **Browser requests to the web UI** (`nestweaver ui`, loopback only) from other sites: DNS rebinding, CSRF, and XSS
  through indexed content rendered in the UI.
- **Other local users on a shared host**, against the daemon's Unix socket, its state directories and its token files.

Trust boundaries: the operator who starts the daemon and writes `instance.toml` is trusted, and so is anyone holding
the admin token. Holders of the org query token (`--auth-token`) are trusted to read the whole graph but not to
mutate it or to reach the host beyond it. Server mode is single-tenant and meant for a trusted network
(`.github/SECURITY.md`).

Defences to test against, with their homes:

- **Bind policy**: `validate_bind_security` in `crates/nestweaver-daemon/src/server.rs`. A non-loopback bind needs an
  auth token and TLS. The admin token must be set alongside the auth token and must differ from it. Webhook secrets
  are at least `MIN_WEBHOOK_SECRET_LEN` bytes.
- **Bearer auth**: constant-time comparisons in `crates/nestweaver-daemon/src/auth.rs` (gRPC),
  `crates/nestweaver-mcp/src/http.rs` (MCP HTTP) and `crates/nestweaver-web/src/routes/admin.rs`. `MUTATING_TOOLS` in
  `http.rs` need the admin token and are refused on read-only replicas. Per-repo authorization is in
  `crates/nestweaver-engine/src/authz.rs` and is meant to fail closed.
- **Webhooks**: `crates/nestweaver-daemon/src/webhook.rs`. GitHub `X-Hub-Signature-256` HMAC, Gitea HMAC, GitLab
  token, and secret rotation. The payload's repository URL must pass the SSRF guard and the config allowlist.
- **Remote git**: `crates/nestweaver-engine/src/ssrf.rs` (`guard_git_url`). It allows https, http and ssh only,
  rejects internal and encoded addresses, fails closed on DNS errors, and pins the resolved IP for http(s) with
  redirects off. `apply_git_isolation` in `crates/nestweaver-engine/src/git_cmd.rs` drops system and global git config,
  prompts and credential helpers, and disables a repository's fsmonitor, hooks, auto-maintenance and `core.askPass`.
  `isolated_git_in` additionally lists the repository's clean/smudge/process filter, textconv and external-diff driver
  config without running anything and blanks every program it names, failing closed when that listing fails. Indexing,
  watching, the working-tree diffs behind `affected-tests --base-ref`, `pr-impact --base`, `blast-radius` and
  `pre-push-impact --local-changes`, and MCP tool calls should therefore run no program a repository's own git config
  names.
- **Indexing limits**: `crates/nestweaver-engine/src/index_limits.rs` caps source files at 2 MiB and notes at 1 MiB by
  default, with 64 MiB ceilings. The walker honours `.gitignore` and does not follow symlinks.
- **Indexing roots**: `brain_add_source` and watch roots are checked by `is_unsafe_index_root` and `watch_path_allowed`
  in `server.rs`. With a config they are limited to its allowlist; without one, system roots and `$HOME` itself are
  refused.
- **Regex**: `crates/nestweaver-store/src/regex.rs` caps patterns at 4096 bytes and the compiled-program and DFA sizes.
- **Graph queries**: values are passed to LadybugDB as bound `$parameters`. String-built Cypher is meant to
  interpolate only constants and integers.
- **Web UI**: `crates/nestweaver-web/src/hardening.rs`. Host must be loopback (or in `NESTWEAVER_UI_ALLOWED_HOSTS`),
  Origin must match Host, security headers are set, and there is no CORS layer on purpose. `/api/v1/source` rejects
  `..`, absolute paths and NUL.

## Components that matter most / least

Most important, in this order:

1. The server-mode network surface: `crates/nestweaver-daemon` (`server.rs`, `auth.rs`, `webhook.rs`, `acme.rs`),
   `crates/nestweaver-mcp` (`http.rs`, `tools.rs`), `crates/nestweaver-web` (admin, auth, metrics routes), and the
   TLS setup in `crates/nestweaver-engine/src/tls.rs`.
2. Anything an MCP tool call or an indexed file can turn into file access outside the indexed roots, a command or git
   argument, a network request, or a graph-query fragment: `crates/nestweaver-engine` (`ssrf.rs`, `git_cmd.rs`,
   `bare_clone.rs`, `index.rs`, `authz.rs`), `crates/nestweaver-store` (query building, `regex.rs`), and
   `crates/nestweaver-storage` (S3 and GitLab backends).
3. Parsers fed with hostile files: `crates/nestweaver-parser` and `crates/nestweaver-resolver`, including frontmatter
   YAML, and the in-repo C FFI (`crates/nestweaver-store/src/zstd.rs`).
4. The web UI and its frontend (`crates/nestweaver-web`, `crates/nestweaver-web/frontend`), where indexed content is
   rendered, and `crates/nestweaver-wasm`, which decodes a MessagePack graph snapshot in the browser.
5. Local multi-user concerns: socket and state-directory paths and permissions in
   `crates/nestweaver-daemon/src/lifecycle.rs` and `crates/nestweaver-client`.

The crates do not forbid `unsafe`. Most of the `unsafe` code is libc process and lock management in `lifecycle.rs`,
`src/main.rs` and `crates/nestweaver-client/src/autostart.rs`. A memory-safety flaw wholly inside LadybugDB,
tree-sitter or a grammar, tantivy, candle or another dependency belongs to that project. Report it here only when
NestWeaver makes it reachable from untrusted input, and show the path.

Out of scope: `app/` (macOS launcher), `npm/` (binary-distribution wrapper), `extensions/`, `integrations/`, `skills/`,
`workspace/`, `scratch/`, `benches/`, `benchmarks/`, `examples/`, `tests/`, `testdata/`, `docs/`, CI configuration,
and the `release-fixture-hooks` and `ci-direct-tests` features, which are never enabled in distributed builds.

## How to exercise it

The image has loopback networking only. Every crate is in the cargo cache, `CARGO_NET_OFFLINE` is set, and
`/src/target/debug` already holds the workspace build and its test binaries. `nestweaver` on `PATH` is the
default-features debug CLI, and `nestweaver --help` lists every command.

- **Index something hostile**: put files in a directory, then run
  `nestweaver index --repo <dir> --db /tmp/nw/brain.lbug` for code, or
  `nestweaver brain add <dir> --db /tmp/nw/brain.lbug` for a Markdown vault. Then query with `nestweaver search <q>`,
  `nestweaver context <name>` or `nestweaver brain search <q>`, adding `--json` and the same `--db`. Always pass
  `--db`: commands otherwise use `$NESTWEAVER_DB` or `./nestweaver.lbug`. `testdata/` holds sample repositories and
  vaults.
- **Server mode**: start it with
  `nestweaver daemon run --server --bind 127.0.0.1:9378 --db /tmp/nw/brain.lbug --auth-token <a> --admin-token <b> --webhook-secret <16+ bytes>`.
  MCP-over-HTTP, the webhook, the admin API and `/metrics` then listen on `127.0.0.1:9379`. Drop the tokens to test
  the tokenless loopback configuration. `curl`, `jq` and `openssl` are available. `--tls-cert`/`--tls-key` with a
  self-signed pair exercise TLS. `docs/server-mode.md` documents the surface.
- **MCP over stdio**: `nestweaver mcp --db /tmp/nw/brain.lbug`, speaking JSON-RPC on stdin and stdout.
- **Web UI**: `nestweaver ui --db /tmp/nw/brain.lbug --port 3000 --no-open`.
- **Embeddings**: no model is cached in the image and there is no network, so semantic search is unavailable and
  keyword and graph retrieval are what run. Do not report the absence of embeddings.
- **Tests**:
  - Unit tests: `NESTWEAVER_NO_DAEMON=1 NESTWEAVER_ALLOW_NO_DAEMON=1 cargo test --locked --workspace --features ci-direct-tests --lib`.
  - The root integration tests (`tests/*.rs`) also expect CI runner markers. Add
    `GITHUB_ACTIONS=true RUNNER_TEMP=/tmp RUNNER_OS=Linux GITHUB_RUN_ID=1` to the line above, drop `--lib`, and add
    `-- --skip daemon_` as CI does.
  - The property tests are `crates/nestweaver-parser/tests/proptest_parse.rs` and
    `crates/nestweaver-resolver/tests/proptest_resolve.rs`.
  - Do not run `cargo test` with `--all-features`: the `metal` feature is macOS-only.
  - The image runs as root. A few tests simulate an unreadable or read-only file or directory with permission bits,
    which root bypasses, so they fail here (for example `tls::tests::a_failed_install_leaves_the_previous_bundle_untouched`
    and `db::engine_format_open_tests::a_failed_create_leaves_no_engine_format_claim`). They pass as a normal user and
    are not findings.

## How you rate severity

A finding counts when someone outside the trust boundaries above can make NestWeaver misbehave through a documented
interface. State the configuration the reproducer needs. A finding against the defaults (tokenless loopback daemon,
stdio MCP, default config) weighs more than one that needs an unusual setting.

- **Critical**: remote code execution or arbitrary file write from a network peer, a webhook, an MCP tool call or an
  indexed file. Bypassing the bearer-token, admin-token or webhook-signature checks on a server bound to a
  non-loopback address. Memory corruption reachable from indexed content or the network.
- **High**:
  - Reading files outside the indexed roots, or leaking tokens and secrets, through MCP tools, the web UI or the APIs.
  - SSRF or git option or argument injection through a repository URL or webhook.
  - Graph-query injection that changes data or reads past per-repo authorization.
  - An org-token holder performing a mutating or admin action.
  - Cross-origin or DNS-rebinding access to the loopback web UI or the MCP-over-HTTP listener that reads graph content
    or triggers a mutation.
  - Stored XSS in the web UI from indexed content.
  - A crash of the daemon by an unauthenticated network peer.
- **Medium**:
  - Denial of service from one hostile repository, note or MCP argument that bypasses the documented limits: the
    indexing size caps, the regex limits and the webhook body limit. Examples are a hang, unbounded memory or a crash
    during indexing.
  - Local privilege issues on a multi-user host, such as other users reaching the daemon socket or reading
    state-directory files.
  - Information disclosure to an unauthenticated peer beyond `/metrics` on a tokenless loopback.
- **Low**:
  - A panic confined to one request or one file.
  - Parsing differentials with no security consequence.
  - Problems that need the admin token, the operator's config, or a non-default option meant for trusted peers.

## How to report

- Give a reproducer that runs in this image: the files to index or the exact request (`curl` command, MCP JSON-RPC
  message or gRPC call), the `nestweaver` command line and config, and what you observed.
- Propose a minimal patch against `main` with a regression test, following the repository's Conventional Commits
  style. Keep unrelated refactoring out of it.
- Report one root cause once, listing every path that reaches it.

## Anything to leave alone

- The known residuals listed in `.github/SECURITY.md`:
  - the `ssh://` clone DNS-rebinding window;
  - backup-archive (`.nwsnap.zst`) expansion on restore, which is a local operation on a trusted file;
  - `git rev-parse <ref>` without `--`;
  - the advisory-flagged transitive dependencies listed there, including the `zip` advisories ignored in
    `.cargo/audit.toml`.
- The Unix-domain socket gRPC service has no authentication by design. Access control is the socket's file
  permissions and its private directory, so report a way for another user to reach it, not the absence of a token.
- A tokenless daemon on loopback trusts local processes. Report cross-origin or other-user reachability, not the
  fact that a local process of the same user can call it.
- With no `instance.toml`, `brain_add_source` can index any absolute path except system roots and `$HOME` itself.
  That is the single-user default; report escapes of the allowlist when a config is present, or of the denylist.
- Anyone holding the org query token can read the whole indexed graph (`.github/SECURITY.md`).
- Neutralising a repository's filter drivers means a file such a driver (for example Git LFS installed with
  `--local`) would have normalised can read as modified in change detection. Over-selection of that kind is expected;
  program execution during `index`, `watch`, a working-tree diff, the daemon or an MCP tool call is in scope.
- Missing rate limiting, and resource use proportional to the size of what the operator chose to index.
- Behaviour that needs the `ci-direct-tests` or `release-fixture-hooks` features, or the macOS-only `metal` feature.
