# Changelog

## [0.8.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.7.0...nestweaver-v0.8.0) (2026-05-28)


### Features

* **cli:** add --allow-writes flag to setup command ([62d06e2](https://github.com/Kehl-io/nestweaver/commit/62d06e2587470d5ce7d7b2adfee1cfecdb60f9af))
* **cli:** add --tools flag for MCP tool allowlisting ([9d3c419](https://github.com/Kehl-io/nestweaver/commit/9d3c4191e7a7ae42065230a2e64be43e79907be3))
* **cli:** expose materialize-projects and detect-implicit-projects as subcommands ([1c03b29](https://github.com/Kehl-io/nestweaver/commit/1c03b29fe0aea75c62637cd3e8a33e56b5a6378f))
* **cli:** restore materialize-projects and detect-implicit-projects subcommands ([a86c787](https://github.com/Kehl-io/nestweaver/commit/a86c7877e5f211e4884bf251e879e578fde089e0))
* **engine:** add .brainignore support for vault indexing exclusion patterns ([9effc18](https://github.com/Kehl-io/nestweaver/commit/9effc18f741d557520232232e72db31de4f32cfd))
* **engine:** add HTML-to-markdown conversion for wiki content ingestion ([28b071f](https://github.com/Kehl-io/nestweaver/commit/28b071f3f366925b44a18cc21790821228d9be43))
* **mcp:** add intent to project_context, tool allowlist, and section title indexing ([36b238e](https://github.com/Kehl-io/nestweaver/commit/36b238e22d07b89b7f45f058d640792f315e90a4))
* **mcp:** wire intent through brain_context and cache get_summary in sidecar ([626918b](https://github.com/Kehl-io/nestweaver/commit/626918b10e8347d15c179b8f3c9f3ae8d6254194))
* **parser:** add JSX component edges and confidence-aware dead code BFS ([965e4c7](https://github.com/Kehl-io/nestweaver/commit/965e4c74bcef917e24a252b753b6e611263933fa))
* **parser:** enrich symbol extraction with constants, properties, types, and expanded queries ([201c27d](https://github.com/Kehl-io/nestweaver/commit/201c27d67b7a292f29e3fdbfc405f015518c2298))
* **resolver:** add file-level IMPORTS edges for every resolved import ([7ce74dc](https://github.com/Kehl-io/nestweaver/commit/7ce74dcfa7ee49296fc0554aedafd64963086fda))
* **resolver:** add monorepo workspace package and tsconfig path alias resolution ([2eff955](https://github.com/Kehl-io/nestweaver/commit/2eff955fe63bc7e1970f9f8bc1665de410a93a38))
* **resolver:** create IMPORTS edges from resolved import references ([5e65cf5](https://github.com/Kehl-io/nestweaver/commit/5e65cf5314617a465b8a2b451201a6f82c8ae4ec))
* **schema:** add USES and ACCESSES edge types with PPR weighting and workspace resolution ([8ab7c21](https://github.com/Kehl-io/nestweaver/commit/8ab7c21a57a00e2ad3870569b11d0f5efc212abb))
* **store:** add upsert helpers for idempotent project materialization ([1c24c4f](https://github.com/Kehl-io/nestweaver/commit/1c24c4ffaaab90a1f4c733003c26985d72b574dd))


### Bug Fixes

* address all code review low and nit findings ([03684dd](https://github.com/Kehl-io/nestweaver/commit/03684dd457b3d84ebf86d7618d84a48fa9ddef2e))
* auto-populate Tantivy after brain add/refresh, fix remaining user issues ([7ab2872](https://github.com/Kehl-io/nestweaver/commit/7ab2872152a89806477a6641936a2a36745e410f))
* auto-populate Tantivy after brain add/refresh, implement wiki refresh timer, fix WAL flush ([2f1c168](https://github.com/Kehl-io/nestweaver/commit/2f1c16857103c707bb69980531e402621d14ff95))
* **bench:** add intent parameter to brain benchmark ([bbd2d18](https://github.com/Kehl-io/nestweaver/commit/bbd2d185eaa9b77cdb19a13170565c4de4c3518b))
* CLI alias resolver, SIGTERM handling, clusters exit code, brain status timestamps ([fb036eb](https://github.com/Kehl-io/nestweaver/commit/fb036eb340cce95d57978829b02a41176ebe4eca))
* **cli:** restore --allow-writes flag on setup command ([d08e936](https://github.com/Kehl-io/nestweaver/commit/d08e9367445c6072c4bb6b2e997bdc5063a41e81))
* **cli:** thread --token-budget through project-context command ([d6a2136](https://github.com/Kehl-io/nestweaver/commit/d6a2136d492f3c0d6285396377e397bb0289d9d1))
* **engine:** boost project-scoped content in PPR ranking for project_context queries ([5bcebeb](https://github.com/Kehl-io/nestweaver/commit/5bcebebabcf0e6e6a420262ee4dd5380bf0281aa))
* **engine:** improve dead code detection accuracy with IMPORTS edge traversal and entry point coverage ([b7bd060](https://github.com/Kehl-io/nestweaver/commit/b7bd060b512fcbc4c51c8ab78ccf1daca62ef6c4))
* **engine:** reduce dead code false positives with type exclusion, dedup, and manifest entry points ([a2f5431](https://github.com/Kehl-io/nestweaver/commit/a2f5431701b250de1d071ba80160a3c0a8c82539))
* **mcp:** correct project UID lookup in project_context tool ([93a8192](https://github.com/Kehl-io/nestweaver/commit/93a8192c8def0b91e3ab131fcb5a069b443005da))
* **mcp:** strip seeds array from default response to respect token budget ([6caf8f8](https://github.com/Kehl-io/nestweaver/commit/6caf8f885c705b9decc09e1a4511157719905387))
* **mcp:** use ProjectContext intent as default for project_context tool ([65babbe](https://github.com/Kehl-io/nestweaver/commit/65babbef482852914b7b59cf1b7764436a66718f))
* **parser:** improve entry point detection for React, Next.js, and frontend frameworks ([ef9dd13](https://github.com/Kehl-io/nestweaver/commit/ef9dd13f9a775dd126ccc513b66803bcd1f35428))
* **parser:** use TSX grammar for .tsx files to fix JSX parse failures ([042f6ee](https://github.com/Kehl-io/nestweaver/commit/042f6ee5d9e18722dec009bbe1627726f5245a59))
* restore project-context results, reject wiki fetch errors, restore CLI subcommands ([47031e1](https://github.com/Kehl-io/nestweaver/commit/47031e1c3328356c083273251acc791cae4e5b45))
* **store:** add missing PROJECT rel tables and confidence column to schema ([fc2c55d](https://github.com/Kehl-io/nestweaver/commit/fc2c55ddd1865fd9ea6c3fd46a2b3c7dfd9069bf))
* **store:** add missing USES and ACCESSES rel tables to schema bootstrap ([6ca9bbf](https://github.com/Kehl-io/nestweaver/commit/6ca9bbf4da0cbe1e55e16fabfec6aaff6545fad7))
* **store:** add read-only Tantivy mode to prevent writer lock blocking searches ([1ecb7ec](https://github.com/Kehl-io/nestweaver/commit/1ecb7ec449ec15afd5497998bb0fcd5e470ac8db))
* **store:** run schema migrations on open() for v0.7 DB compatibility ([fb5e300](https://github.com/Kehl-io/nestweaver/commit/fb5e3004942c248306212bed7572b4b13e9fab3f))


### Performance Improvements

* **store:** wrap bulk index writes in transactions for 12x speedup ([0422b43](https://github.com/Kehl-io/nestweaver/commit/0422b43481c2ba3c82ba3518a01ab744d3275bcc))

## [0.7.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.6.0...nestweaver-v0.7.0) (2026-05-27)


### Features

* **engine:** add dead code detection via entry point reachability ([5f4221c](https://github.com/Kehl-io/nestweaver/commit/5f4221c81fa807f4a4ec6f6e8a321cf1a3e0b698))
* **engine:** add graph export to Cypher, GraphML, and Mermaid ([0a9e5f8](https://github.com/Kehl-io/nestweaver/commit/0a9e5f8e24b5b21795cb9a824f9f492a71267862))
* **engine:** add hierarchical code summaries for token-efficient retrieval ([9a26820](https://github.com/Kehl-io/nestweaver/commit/9a268209e2d31d424181646e33cf748bfac7e9b7))
* **engine:** add hub and bridge node detection ([30de53d](https://github.com/Kehl-io/nestweaver/commit/30de53d2c9be7da84c8d15e6a1d8e14051ecaa87))
* **engine:** add PR blast radius analysis with risk scoring ([70e2b91](https://github.com/Kehl-io/nestweaver/commit/70e2b9119e14444f1fda2bfac0b4857dd6eab65e))
* **engine:** add setup support for 10 additional AI tools ([1daf7d2](https://github.com/Kehl-io/nestweaver/commit/1daf7d213395894953efab33cea22eea9748d2e7))
* **engine:** generate SKILL.md conforming to Agent Skills standard ([a4c2e38](https://github.com/Kehl-io/nestweaver/commit/a4c2e38798698cb0fce8830d0a4cc62ff524bcb4))
* **mcp:** improve tool descriptions and add response_format parameter ([d178db1](https://github.com/Kehl-io/nestweaver/commit/d178db1aed20d6d29684d2e7eb0a4f48977ce691))
* **parser:** add Julia, SQL, HCL, Fortran, and Pascal language support ([9638978](https://github.com/Kehl-io/nestweaver/commit/9638978fef35ca8797cbfb1fe3c5a63351ab80d2))
* **parser:** add Lua, Bash, Scala, and Elixir language support ([d3556c1](https://github.com/Kehl-io/nestweaver/commit/d3556c1cfec308d64c4f6d3772d8b209ebb50ae7))
* **parser:** add Vue, Svelte, Astro, and SystemVerilog language support ([97509e3](https://github.com/Kehl-io/nestweaver/commit/97509e37d72f48b7666079689d2a84ecb6851355))
* **parser:** add Zig, Objective-C, Groovy, and PowerShell language support ([d4472c5](https://github.com/Kehl-io/nestweaver/commit/d4472c584b81b84528eca76d54396b328cafb8ad))
* **store:** dynamic PPR with query-type-aware alpha and edge weighting ([55123a3](https://github.com/Kehl-io/nestweaver/commit/55123a3d062c3eb091a81fca716e300d14587877))


### Bug Fixes

* address all remaining code review findings (14 fixes) ([a6e6777](https://github.com/Kehl-io/nestweaver/commit/a6e6777f0f1e262003667b5f0236e33d40bed6b6))
* address code review findings (12 fixes) ([b8c2e85](https://github.com/Kehl-io/nestweaver/commit/b8c2e857f1f22f9eb895716ae59e6000a694e150))
* **mcp:** update tool count assertion for get_summary tool ([ede630a](https://github.com/Kehl-io/nestweaver/commit/ede630a48988aba66f22e028b9e111d13c331200))

## [0.6.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.5.1...nestweaver-v0.6.0) (2026-05-27)


### Features

* **cli:** add --stats flag and output control flags ([52449fb](https://github.com/Kehl-io/nestweaver/commit/52449fb46b96360e7a7cab9b97dff43c5a9f2f25))
* **engine:** add watch mode for live code re-indexing ([13e2fba](https://github.com/Kehl-io/nestweaver/commit/13e2fba37bb6c93743c87f02992bd2dd537a1d07))


### Bug Fixes

* **ci:** add --allow-multiple-definition for aarch64-linux release builds ([00902cc](https://github.com/Kehl-io/nestweaver/commit/00902cc098df0bd9794e657822264422116cbffd))


### Performance Improvements

* **engine:** tiered change detection for faster incremental indexing ([ca0f4f4](https://github.com/Kehl-io/nestweaver/commit/ca0f4f4d50ced1568b227e46e1c550da97a36273))

## [0.5.1](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.5.0...nestweaver-v0.5.1) (2026-05-27)


### Bug Fixes

* **ci:** install g++ cross-compiler for aarch64-linux release builds ([19ac794](https://github.com/Kehl-io/nestweaver/commit/19ac794767aff49e08a4d4129d059ee9ed7ee3db))

## [0.5.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.4.0...nestweaver-v0.5.0) (2026-05-27)


### Features

* **cli:** add shell completions, miette diagnostics, and zero-config index ([e483aaf](https://github.com/Kehl-io/nestweaver/commit/e483aaf735dad9875ce77b7e5da167671deed0d3))
* **engine:** add progress bars to indexing pipeline ([e328690](https://github.com/Kehl-io/nestweaver/commit/e328690df1de77f1286d4db710ab0cc189fae4c6))


### Bug Fixes

* **ci:** use LBUG_BUILD_FROM_SOURCE for release builds (reliable cross-platform) ([2bbf192](https://github.com/Kehl-io/nestweaver/commit/2bbf192af945d507cb2feedfbfd5c38deba1a424))

## [0.4.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.3.0...nestweaver-v0.4.0) (2026-05-27)


### Features

* **cli:** add standard output control flags ([60b218a](https://github.com/Kehl-io/nestweaver/commit/60b218ad46ce414934836aad8b9c52bd78939dfd))


### Bug Fixes

* **ci:** warm lbug cache for target architecture in release builds ([0c51629](https://github.com/Kehl-io/nestweaver/commit/0c51629063377141b06c6c1ebffd5de6f2c9a565))

## [0.3.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.2.0...nestweaver-v0.3.0) (2026-05-27)


### Features

* add Codex and JetBrains integrations, fix Windsurf config path ([4ed6d10](https://github.com/Kehl-io/nestweaver/commit/4ed6d107ff8379ab0ef4d9357c7c7cc0f64f4da9))
* add npm package for binary distribution (@kehl-io/nestweaver) ([f461c82](https://github.com/Kehl-io/nestweaver/commit/f461c820a55afce4dcb31ff4eb78612f28a85e02))
* **branding:** add fierce geometric raptor logo with full asset suite ([d16ebea](https://github.com/Kehl-io/nestweaver/commit/d16ebea9271a4d561c279f6aee451e4982036c1f))
* **cli:** add nestweaver setup command for auto-configuring AI coding tools ([a158bc5](https://github.com/Kehl-io/nestweaver/commit/a158bc510d21d602e113d9d0e1d927ca927284ee))
* **engine:** add MCP client module for calling external MCP servers ([360dde4](https://github.com/Kehl-io/nestweaver/commit/360dde49e29469ac612c3951ca43e40deec691b4))
* **engine:** add multi-format guide generation (skill, cursor-rule, agents-md) ([1e579a5](https://github.com/Kehl-io/nestweaver/commit/1e579a5b204a94a365f9cf8a8fb0e30038b9fe7b))
* **engine:** add ProjectConfig, WikiSourceConfig, and McpServerConfig to instance config ([ab5338a](https://github.com/Kehl-io/nestweaver/commit/ab5338a2797d7f36c152e82667d492d061992a8c))
* **engine:** add Projects section to generated codebase guide ([b106df2](https://github.com/Kehl-io/nestweaver/commit/b106df21fd0d5cef605ec578145cc16789c31626))
* **engine:** decompose wiki notes into headings and sections after ingestion ([7cdef14](https://github.com/Kehl-io/nestweaver/commit/7cdef148449d634fdfb5729cdb89e87a68c6290b))
* **engine:** implement project materialization with explicit and implicit declaration ([2389472](https://github.com/Kehl-io/nestweaver/commit/2389472acc4348febe1f93c65952fbf02e7c3065))
* **engine:** ingest wiki sources via MCP client calls during project materialization ([38bc3b0](https://github.com/Kehl-io/nestweaver/commit/38bc3b0862a924b584a018cd554a1ce709c42836))
* **integrations:** add SessionStart hook and PreToolUse blast radius enrichment for Claude Code ([911d9b6](https://github.com/Kehl-io/nestweaver/commit/911d9b639d826441243f0d1f0c5eadbd5d7edc12))
* **mcp,cli:** add project_context tool, list-projects command, and project seed expansion ([ee01c63](https://github.com/Kehl-io/nestweaver/commit/ee01c63d06cf4236b1c5c7c5c88c41687f0aec3f))
* **mcp,cli:** add tags and exclude_tags filters to brain_context ([ea36168](https://github.com/Kehl-io/nestweaver/commit/ea361689d06eed2006a39d1fa3267770ebd9390e))
* **mcp:** add --lite mode exposing 6 core tools for tool-capped environments ([495e7b0](https://github.com/Kehl-io/nestweaver/commit/495e7b0b4aed1930f62640646229a052c6004006))
* **store:** add Project edge types and read/write methods ([12fd7e3](https://github.com/Kehl-io/nestweaver/commit/12fd7e367dffaec72b114f26a68302a933739cd3))
* **store:** add Project nodes and edges to unified PPR graph scope ([4b3966c](https://github.com/Kehl-io/nestweaver/commit/4b3966c2642f9044a597c60908e8ea09a9379bca))
* **vscode:** add CodeLens, [@nestweaver](https://github.com/nestweaver) chat participant, status bar, and graph enrichment ([fb4699b](https://github.com/Kehl-io/nestweaver/commit/fb4699b6ab3dd8caaadb42c35b9460cd261e0d7d))


### Bug Fixes

* add missing MCP schema params, fix timezone parsing, extract shared recency utils ([3ba9be5](https://github.com/Kehl-io/nestweaver/commit/3ba9be5a84e451d7958199324b700c1e74d6a074))
* address review findings (hook command, install URLs, JSON safety, CodeLens caching) ([428f2c3](https://github.com/Kehl-io/nestweaver/commit/428f2c3ef56ce9a8b0ca953089c908088b04d661))
* **ci:** add build deps and lbug cache warming to release build jobs ([b82ca97](https://github.com/Kehl-io/nestweaver/commit/b82ca97ca58f5e73e491e0d07cc82427d3140a1c))
* **ci:** add cargo build step before tests for lbug prebuilt download ([b189912](https://github.com/Kehl-io/nestweaver/commit/b189912338c1b714a565d5b5376269ee1769175a))
* **ci:** add linker config to clippy job and make audit advisory (non-blocking) ([d5ffaf2](https://github.com/Kehl-io/nestweaver/commit/d5ffaf2128275d9a67ad4bf2b61d846047244c06))
* **ci:** build nestweaver-store first to cache lbug prebuilt before full workspace build ([3e50674](https://github.com/Kehl-io/nestweaver/commit/3e50674853a51434a820e10959203097237cb200))
* **ci:** build test targets to ensure lbug library is available for test linking ([1173695](https://github.com/Kehl-io/nestweaver/commit/1173695706c331e3bb227e795673a22a4320a69a))
* **ci:** configure release-please for cargo workspace and update repo URLs to Kehl-io ([f6e7e3c](https://github.com/Kehl-io/nestweaver/commit/f6e7e3cf46a48ce12f41747c748c4909986bc9c6))
* **ci:** install cmake and g++ for lbug native library build ([146be75](https://github.com/Kehl-io/nestweaver/commit/146be75e38dc5b2de2884f83d34352b78566f711))
* **ci:** pre-download liblbug before build (LadybugDB recommended pattern) ([6eab138](https://github.com/Kehl-io/nestweaver/commit/6eab138352238cd35019c3e101d35e67037830b1))
* **ci:** retry build after lbug prebuilt download race condition ([a0752d7](https://github.com/Kehl-io/nestweaver/commit/a0752d7c5759da67bfe5af7b107c5bef71deaac9))
* **ci:** run cargo build before clippy to trigger lbug prebuilt download ([1e83655](https://github.com/Kehl-io/nestweaver/commit/1e83655a6a524f1fe625841e3531e28725d5ce85))
* **ci:** show full build output for lbug debugging ([a08dbae](https://github.com/Kehl-io/nestweaver/commit/a08dbae8d5441a940308c0cd413afa41c284448f))
* **ci:** simplify release-please config and document required repo permissions ([1c8ba16](https://github.com/Kehl-io/nestweaver/commit/1c8ba166542f099131660644bb2173472e8098aa))
* **ci:** switch release-please to simple type for workspace.package.version compatibility ([30461cc](https://github.com/Kehl-io/nestweaver/commit/30461cc10d2f047f65bfbb6bfbd7ce4ddc65290d))
* **cli:** auto-wire manifests path in brain watch for manifest auto-refresh ([93a94b1](https://github.com/Kehl-io/nestweaver/commit/93a94b112bb3db69d4e16abb7a0682438f8c75fa))
* **engine:** align sidecar paths between snapshot and index for pagerank/manifests ([a39de32](https://github.com/Kehl-io/nestweaver/commit/a39de32f3fba2bc01f7378844c1f4266bfce8922))
* **engine:** clean existing project edges before re-materialization ([7516162](https://github.com/Kehl-io/nestweaver/commit/7516162e5676824a5037be40da678095344bcc55))
* **engine:** fix McpClient BufReader recreation and add notification skipping ([bde2ec3](https://github.com/Kehl-io/nestweaver/commit/bde2ec316701b8b3d18796f770622dcd0fadfc28))
* **engine:** use atomic writes for extension store sidecar ([0acfcd8](https://github.com/Kehl-io/nestweaver/commit/0acfcd8759b2d300f16092533f9619196c5d8bae))
* **engine:** wire project alias resolution through extension sidecar ([ae8b7f4](https://github.com/Kehl-io/nestweaver/commit/ae8b7f468f1286156ea9c45fe693670172a665f8))
* **mcp,cli:** wire alias sidecar into brain_context for automatic seed resolution ([4273cf9](https://github.com/Kehl-io/nestweaver/commit/4273cf9f6061ae310a22db8e8268961196e2c354))
* **mcp:** align client protocol version with server ([859b11f](https://github.com/Kehl-io/nestweaver/commit/859b11ffdff216f6d07210a690cbd81a6cb99e91))
* **mcp:** validate hybrid search weights and clamp to non-negative ([62a3ab2](https://github.com/Kehl-io/nestweaver/commit/62a3ab2732cf501030d6731ef7476ccedccba0b5))
* **schema:** add ProjectIncludesNote to EdgeType enum for completeness ([4b702b4](https://github.com/Kehl-io/nestweaver/commit/4b702b4f320ee57f4fecdcfb026275107a22af25))
* **store:** use asymmetric edge weighting in PPR (0.3x for reverse edges) ([5d783d0](https://github.com/Kehl-io/nestweaver/commit/5d783d0deeeccd941071f6cba4ae063f19335a4b))
* **store:** use parameterized queries for project edge lookups ([aa7beba](https://github.com/Kehl-io/nestweaver/commit/aa7beba76ec513fdc016c0a04af51243acb62a3d))
* **ui:** truncate labels at overview zoom instead of hiding them ([41d2db1](https://github.com/Kehl-io/nestweaver/commit/41d2db10b60c3cb4a3eadfdfbb3e0924c5b51ab8))
* **web:** fix E2E test assertions and CI build order for frontend embedding ([a2f2269](https://github.com/Kehl-io/nestweaver/commit/a2f22699f6e09be9d0bb481b8be522a0bb187c61))
* **web:** resolve TypeScript compilation errors ([bf67137](https://github.com/Kehl-io/nestweaver/commit/bf67137c99404d4448c1e2b410359e839bae29c9))
* **web:** transparent dark logo background and theme-aware TopBar ([92dfff5](https://github.com/Kehl-io/nestweaver/commit/92dfff514d7ba4dbf39a2dcf968a356166972b6f))

## [0.2.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.1.0...nestweaver-v0.2.0) (2026-05-27)


### Features

* add Codex and JetBrains integrations, fix Windsurf config path ([4ed6d10](https://github.com/Kehl-io/nestweaver/commit/4ed6d107ff8379ab0ef4d9357c7c7cc0f64f4da9))
* add npm package for binary distribution (@kehl-io/nestweaver) ([f461c82](https://github.com/Kehl-io/nestweaver/commit/f461c820a55afce4dcb31ff4eb78612f28a85e02))
* **branding:** add fierce geometric raptor logo with full asset suite ([d16ebea](https://github.com/Kehl-io/nestweaver/commit/d16ebea9271a4d561c279f6aee451e4982036c1f))
* **cli:** add nestweaver setup command for auto-configuring AI coding tools ([a158bc5](https://github.com/Kehl-io/nestweaver/commit/a158bc510d21d602e113d9d0e1d927ca927284ee))
* **engine:** add MCP client module for calling external MCP servers ([360dde4](https://github.com/Kehl-io/nestweaver/commit/360dde49e29469ac612c3951ca43e40deec691b4))
* **engine:** add multi-format guide generation (skill, cursor-rule, agents-md) ([1e579a5](https://github.com/Kehl-io/nestweaver/commit/1e579a5b204a94a365f9cf8a8fb0e30038b9fe7b))
* **engine:** add ProjectConfig, WikiSourceConfig, and McpServerConfig to instance config ([ab5338a](https://github.com/Kehl-io/nestweaver/commit/ab5338a2797d7f36c152e82667d492d061992a8c))
* **engine:** add Projects section to generated codebase guide ([b106df2](https://github.com/Kehl-io/nestweaver/commit/b106df21fd0d5cef605ec578145cc16789c31626))
* **engine:** decompose wiki notes into headings and sections after ingestion ([7cdef14](https://github.com/Kehl-io/nestweaver/commit/7cdef148449d634fdfb5729cdb89e87a68c6290b))
* **engine:** implement project materialization with explicit and implicit declaration ([2389472](https://github.com/Kehl-io/nestweaver/commit/2389472acc4348febe1f93c65952fbf02e7c3065))
* **engine:** ingest wiki sources via MCP client calls during project materialization ([38bc3b0](https://github.com/Kehl-io/nestweaver/commit/38bc3b0862a924b584a018cd554a1ce709c42836))
* **integrations:** add SessionStart hook and PreToolUse blast radius enrichment for Claude Code ([911d9b6](https://github.com/Kehl-io/nestweaver/commit/911d9b639d826441243f0d1f0c5eadbd5d7edc12))
* **mcp,cli:** add project_context tool, list-projects command, and project seed expansion ([ee01c63](https://github.com/Kehl-io/nestweaver/commit/ee01c63d06cf4236b1c5c7c5c88c41687f0aec3f))
* **mcp,cli:** add tags and exclude_tags filters to brain_context ([ea36168](https://github.com/Kehl-io/nestweaver/commit/ea361689d06eed2006a39d1fa3267770ebd9390e))
* **mcp:** add --lite mode exposing 6 core tools for tool-capped environments ([495e7b0](https://github.com/Kehl-io/nestweaver/commit/495e7b0b4aed1930f62640646229a052c6004006))
* **store:** add Project edge types and read/write methods ([12fd7e3](https://github.com/Kehl-io/nestweaver/commit/12fd7e367dffaec72b114f26a68302a933739cd3))
* **store:** add Project nodes and edges to unified PPR graph scope ([4b3966c](https://github.com/Kehl-io/nestweaver/commit/4b3966c2642f9044a597c60908e8ea09a9379bca))
* **vscode:** add CodeLens, [@nestweaver](https://github.com/nestweaver) chat participant, status bar, and graph enrichment ([fb4699b](https://github.com/Kehl-io/nestweaver/commit/fb4699b6ab3dd8caaadb42c35b9460cd261e0d7d))


### Bug Fixes

* add missing MCP schema params, fix timezone parsing, extract shared recency utils ([3ba9be5](https://github.com/Kehl-io/nestweaver/commit/3ba9be5a84e451d7958199324b700c1e74d6a074))
* address review findings (hook command, install URLs, JSON safety, CodeLens caching) ([428f2c3](https://github.com/Kehl-io/nestweaver/commit/428f2c3ef56ce9a8b0ca953089c908088b04d661))
* **ci:** add cargo build step before tests for lbug prebuilt download ([b189912](https://github.com/Kehl-io/nestweaver/commit/b189912338c1b714a565d5b5376269ee1769175a))
* **ci:** add linker config to clippy job and make audit advisory (non-blocking) ([d5ffaf2](https://github.com/Kehl-io/nestweaver/commit/d5ffaf2128275d9a67ad4bf2b61d846047244c06))
* **ci:** build nestweaver-store first to cache lbug prebuilt before full workspace build ([3e50674](https://github.com/Kehl-io/nestweaver/commit/3e50674853a51434a820e10959203097237cb200))
* **ci:** build test targets to ensure lbug library is available for test linking ([1173695](https://github.com/Kehl-io/nestweaver/commit/1173695706c331e3bb227e795673a22a4320a69a))
* **ci:** configure release-please for cargo workspace and update repo URLs to Kehl-io ([f6e7e3c](https://github.com/Kehl-io/nestweaver/commit/f6e7e3cf46a48ce12f41747c748c4909986bc9c6))
* **ci:** install cmake and g++ for lbug native library build ([146be75](https://github.com/Kehl-io/nestweaver/commit/146be75e38dc5b2de2884f83d34352b78566f711))
* **ci:** pre-download liblbug before build (LadybugDB recommended pattern) ([6eab138](https://github.com/Kehl-io/nestweaver/commit/6eab138352238cd35019c3e101d35e67037830b1))
* **ci:** retry build after lbug prebuilt download race condition ([a0752d7](https://github.com/Kehl-io/nestweaver/commit/a0752d7c5759da67bfe5af7b107c5bef71deaac9))
* **ci:** run cargo build before clippy to trigger lbug prebuilt download ([1e83655](https://github.com/Kehl-io/nestweaver/commit/1e83655a6a524f1fe625841e3531e28725d5ce85))
* **ci:** show full build output for lbug debugging ([a08dbae](https://github.com/Kehl-io/nestweaver/commit/a08dbae8d5441a940308c0cd413afa41c284448f))
* **ci:** simplify release-please config and document required repo permissions ([1c8ba16](https://github.com/Kehl-io/nestweaver/commit/1c8ba166542f099131660644bb2173472e8098aa))
* **ci:** switch release-please to simple type for workspace.package.version compatibility ([30461cc](https://github.com/Kehl-io/nestweaver/commit/30461cc10d2f047f65bfbb6bfbd7ce4ddc65290d))
* **cli:** auto-wire manifests path in brain watch for manifest auto-refresh ([93a94b1](https://github.com/Kehl-io/nestweaver/commit/93a94b112bb3db69d4e16abb7a0682438f8c75fa))
* **engine:** align sidecar paths between snapshot and index for pagerank/manifests ([a39de32](https://github.com/Kehl-io/nestweaver/commit/a39de32f3fba2bc01f7378844c1f4266bfce8922))
* **engine:** clean existing project edges before re-materialization ([7516162](https://github.com/Kehl-io/nestweaver/commit/7516162e5676824a5037be40da678095344bcc55))
* **engine:** fix McpClient BufReader recreation and add notification skipping ([bde2ec3](https://github.com/Kehl-io/nestweaver/commit/bde2ec316701b8b3d18796f770622dcd0fadfc28))
* **engine:** use atomic writes for extension store sidecar ([0acfcd8](https://github.com/Kehl-io/nestweaver/commit/0acfcd8759b2d300f16092533f9619196c5d8bae))
* **engine:** wire project alias resolution through extension sidecar ([ae8b7f4](https://github.com/Kehl-io/nestweaver/commit/ae8b7f468f1286156ea9c45fe693670172a665f8))
* **mcp,cli:** wire alias sidecar into brain_context for automatic seed resolution ([4273cf9](https://github.com/Kehl-io/nestweaver/commit/4273cf9f6061ae310a22db8e8268961196e2c354))
* **mcp:** align client protocol version with server ([859b11f](https://github.com/Kehl-io/nestweaver/commit/859b11ffdff216f6d07210a690cbd81a6cb99e91))
* **mcp:** validate hybrid search weights and clamp to non-negative ([62a3ab2](https://github.com/Kehl-io/nestweaver/commit/62a3ab2732cf501030d6731ef7476ccedccba0b5))
* **schema:** add ProjectIncludesNote to EdgeType enum for completeness ([4b702b4](https://github.com/Kehl-io/nestweaver/commit/4b702b4f320ee57f4fecdcfb026275107a22af25))
* **store:** use asymmetric edge weighting in PPR (0.3x for reverse edges) ([5d783d0](https://github.com/Kehl-io/nestweaver/commit/5d783d0deeeccd941071f6cba4ae063f19335a4b))
* **store:** use parameterized queries for project edge lookups ([aa7beba](https://github.com/Kehl-io/nestweaver/commit/aa7beba76ec513fdc016c0a04af51243acb62a3d))
* **ui:** truncate labels at overview zoom instead of hiding them ([41d2db1](https://github.com/Kehl-io/nestweaver/commit/41d2db10b60c3cb4a3eadfdfbb3e0924c5b51ab8))
* **web:** resolve TypeScript compilation errors ([bf67137](https://github.com/Kehl-io/nestweaver/commit/bf67137c99404d4448c1e2b410359e839bae29c9))
* **web:** transparent dark logo background and theme-aware TopBar ([92dfff5](https://github.com/Kehl-io/nestweaver/commit/92dfff514d7ba4dbf39a2dcf968a356166972b6f))
