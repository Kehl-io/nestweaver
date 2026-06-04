# Changelog

## [0.18.0](https://github.com/Kehl-io/nestweaver/compare/v0.17.0...v0.18.0) (2026-06-04)


### Features

* **cli:** route `brain search` through daemon Search RPC when available ([8bb519d](https://github.com/Kehl-io/nestweaver/commit/8bb519df69364ce4eb5297e700c1a3088f0ad692))
* **daemon:** load InstanceConfig for ranking-prior parity in tool dispatch ([c44ec9c](https://github.com/Kehl-io/nestweaver/commit/c44ec9c6c90a1a957bdb5532cd4bd2486d093766))
* **investigate:** surface body truncation via `body_complete` + newline-aware cuts ([c3cf283](https://github.com/Kehl-io/nestweaver/commit/c3cf283c5d59b4d09642fe8dc5011b3e6eefe2b5))


### Bug Fixes

* **ci:** add --workspace to clippy job and fix pre-existing test errors ([a50b217](https://github.com/Kehl-io/nestweaver/commit/a50b217a0300298c7b885219cf98cedddca028d6))
* **cli:** atomically detect running daemon via pidfile flock in daemon start ([b741b94](https://github.com/Kehl-io/nestweaver/commit/b741b94540e36e5b3adf543a5c12329b9f88e8f2))
* **cli:** silence stop_watch errors when daemon is already gone ([1dc8e3e](https://github.com/Kehl-io/nestweaver/commit/1dc8e3e8b7f4b443e310727de589f9e23df28175))
* **cli:** thread --config through autostart so auto-spawned daemons get ranking priors ([1d11e6d](https://github.com/Kehl-io/nestweaver/commit/1d11e6dcb262b4bf3bd16409ae00b0fa453248c3))
* daemon correctness, search parity, investigate fidelity, and agentic integration improvements ([b98069e](https://github.com/Kehl-io/nestweaver/commit/b98069ee5697890a27f1be4a7b3a76a47ed91e31))
* **engine:** route daemon writer-mode Tantivy index into BrainWatcher ([9681fb5](https://github.com/Kehl-io/nestweaver/commit/9681fb5b985822e993ff16204a524b35f495486a))
* **search:** treat `limit` as per-kind so symbols stop being squeezed out ([f37a0bd](https://github.com/Kehl-io/nestweaver/commit/f37a0bd3a51dbd9eb6308aff4945c514eebddb11))

## [0.17.0](https://github.com/Kehl-io/nestweaver/compare/v0.16.0...v0.17.0) (2026-06-04)


### Features

* edge evidence arrays + full language parity (27 tree-sitter, 19 type queries) ([c7a955d](https://github.com/Kehl-io/nestweaver/commit/c7a955d7e636619a57ea5c295adc6c43b9455867))
* **export:** read and surface edge evidence in cypher export ([4564879](https://github.com/Kehl-io/nestweaver/commit/456487924926827d54cdbc6da499bfdb3b6c7dd8))
* **parser:** add C/Elixir type queries, fix Swift receivers, add Lua self binding ([be91ebc](https://github.com/Kehl-io/nestweaver/commit/be91ebc0ff478dc57aee3648c8bb1ddc7ccfd146))
* **parser:** add constructor patterns to Kotlin, Dart, Swift, Scala, C#, SystemVerilog type queries ([8f6703e](https://github.com/Kehl-io/nestweaver/commit/8f6703eeae64d1683e3ab65cd377a6824babab2f))
* **parser:** add Go interface methods, array types + Python class attributes, instance properties ([59afd0b](https://github.com/Kehl-io/nestweaver/commit/59afd0b0de0d8bc0119da275c476d5d14b715104))
* **parser:** add interface method declarations and instance property captures across languages ([3ad76ae](https://github.com/Kehl-io/nestweaver/commit/3ad76aec180cc399e24c7a726a44acabebf4180b))
* **parser:** add type extraction queries for C++, C#, Kotlin, PHP, Dart, Swift, Scala, Ruby ([723b6ac](https://github.com/Kehl-io/nestweaver/commit/723b6acb7f9879b137bc33f0c9b43f64053a0b77))
* **parser:** add type queries and self bindings for remaining OOP languages ([5304c7b](https://github.com/Kehl-io/nestweaver/commit/5304c7b97ab9c987bbfe2b00e58a8f90cbeb6a70))
* **parser:** add visibility inference for Ruby, PowerShell, Fortran, Pascal, SystemVerilog, Julia ([fd2a360](https://github.com/Kehl-io/nestweaver/commit/fd2a36016093b26c052a5bb8b003e904c8c620b7))
* **parser:** expand receiver extraction to all languages for type_aware resolution ([e633458](https://github.com/Kehl-io/nestweaver/commit/e633458a12dd423b3f403395284cd026cda54842))
* **parser:** extend parent_name to fields/properties for MEMBER_OF expansion ([3727cc9](https://github.com/Kehl-io/nestweaver/commit/3727cc977d56af69c4c4c3643fd808662a56cd7e))
* **parser:** upgrade Fortran, Pascal, SystemVerilog from regex to tree-sitter ([47fc325](https://github.com/Kehl-io/nestweaver/commit/47fc32535c3a69f77b7eb77f5037a3299bb143c6))
* **parser:** upgrade Groovy, Zig, Objective-C from regex to tree-sitter ([66e0353](https://github.com/Kehl-io/nestweaver/commit/66e0353c10613543dcc44cc602457f5628390e99))
* **parser:** upgrade PowerShell, Julia, SQL, HCL from regex to tree-sitter ([c79c268](https://github.com/Kehl-io/nestweaver/commit/c79c268d7db4288edb4ab43a4009dffd875d751a))
* **resolver:** add import resolvers for Scala, Groovy, Fortran, Pascal, SystemVerilog, Zig, ObjC, Lua, PowerShell, Julia ([1e44272](https://github.com/Kehl-io/nestweaver/commit/1e44272d962f3b9e0be306caeba9af6d7d9e4d4a))
* **resolver:** improve assignment extraction with dotted paths and function calls ([9ac9e4f](https://github.com/Kehl-io/nestweaver/commit/9ac9e4f3d0cb1a02a6d686d9886fbe8f08843535))
* **resolver:** populate evidence array at each resolution step ([0ecd153](https://github.com/Kehl-io/nestweaver/commit/0ecd153584b4d2cc0837b1294a1edabc039d5760))
* **schema:** add EdgeEvidence struct and evidence field to ResolvedEdge ([b4513aa](https://github.com/Kehl-io/nestweaver/commit/b4513aa1a7ff0b34f52bfff5f106464cb7415d18))
* **store:** persist edge evidence as JSON property ([a268bcd](https://github.com/Kehl-io/nestweaver/commit/a268bcdfbb0eff90bd421397cd21591d908d37ae))
* **store:** surface FILE_HAS_SYMBOL as DEFINES edges in typed edge export ([049d745](https://github.com/Kehl-io/nestweaver/commit/049d745793d81e681d6a3ead2a5b93f51a5247b4))


### Bug Fixes

* add evidence to store/ranking tests, remove unused tree-sitter-sql dep ([b03a828](https://github.com/Kehl-io/nestweaver/commit/b03a8281b35f81af87be9834d5af6942620fabe6))
* address all minor review findings — deterministic resolvers, Ruby visibility docs, ObjC fallback docs, fix dotted assignment test ([9cac397](https://github.com/Kehl-io/nestweaver/commit/9cac397873e8d86491ce4d68901fe0c61a5639eb))
* address code review findings — remove misleading Julia visibility, fix self-referential assignments, remove redundant SV check ([3b593f2](https://github.com/Kehl-io/nestweaver/commit/3b593f2a73540277822bf411453ab5acca1bc8ef))
* implement real Elixir type extraction and re-enable HCL tree-sitter ([1a42390](https://github.com/Kehl-io/nestweaver/commit/1a42390ec03d4523e584666a42064bd544998226))
* resolve clippy warnings and remove dead regex parser files ([194d79a](https://github.com/Kehl-io/nestweaver/commit/194d79ad05f0167149e2638f20ad4f1c17827ca0))
* **resolver:** deterministic import resolution — prefer shortest path on ambiguous matches ([e845cb8](https://github.com/Kehl-io/nestweaver/commit/e845cb85b61c4b2344c2f92c156c20c30b286bc5))
* revert HCL to regex parser, fix Swift types query, relax Julia/PowerShell tests ([60d5bb7](https://github.com/Kehl-io/nestweaver/commit/60d5bb7268dbb99797ce47972429805e82b24617))
* rustfmt formatting ([d32025d](https://github.com/Kehl-io/nestweaver/commit/d32025d705c0030e7728984c97f4c7bf138f6715))
* rustfmt imports.rs ([ea2b21a](https://github.com/Kehl-io/nestweaver/commit/ea2b21a6fce7ca555a10edc1ffe42f216b60d1cd))
* use TypedEdge type alias to satisfy clippy complexity lint ([a881c47](https://github.com/Kehl-io/nestweaver/commit/a881c4728be7c9df20508194025c800e095a7ae1))

## [0.16.0](https://github.com/Kehl-io/nestweaver/compare/v0.15.0...v0.16.0) (2026-06-02)


### Features

* add watch config, simplify daemon lock error, apply lint fixes ([f9e1bf9](https://github.com/Kehl-io/nestweaver/commit/f9e1bf9129fac29101a5661c4c90e3381f5d21b3))
* **cli:** route brain watch through daemon when use_daemon=true ([78e2a1d](https://github.com/Kehl-io/nestweaver/commit/78e2a1d5955d3eaaae2e09dc77b018bba9877b82))
* **daemon:** implement WatchVault and StopWatch RPCs ([199cab1](https://github.com/Kehl-io/nestweaver/commit/199cab18e034f2f2a169044d1494463a0dd5abb7))
* implement stubbed features, daemon-subsumes-watcher, production hardening ([5542a6e](https://github.com/Kehl-io/nestweaver/commit/5542a6e0324ec4f67a2218a8908a8fbc19f879a9))
* **index:** emit MEMBER_OF edges from parent_name during indexing ([2f516a9](https://github.com/Kehl-io/nestweaver/commit/2f516a9ec03e3548125a0e0ff7359322f70b8f72))
* **parser:** expand Rust type queries with struct constructors and destructuring ([533feec](https://github.com/Kehl-io/nestweaver/commit/533feec02bfaf6c1d260dd4d0f7eecbbabb59539))
* **parser:** expand type queries for TS, Python, Go, Java ([e29b916](https://github.com/Kehl-io/nestweaver/commit/e29b9163e8c8c01909a97a79eea91d15e8e06cc7))
* **proto:** add WatchVault and StopWatch RPCs ([90b04c2](https://github.com/Kehl-io/nestweaver/commit/90b04c2709bc22b321352e11162e571b0c4ef4de))
* **resolver:** decompose chained dot receivers for type-aware resolution ([66be11d](https://github.com/Kehl-io/nestweaver/commit/66be11d9d4ec7b6be4293c49479228b2fc767fcd))


### Bug Fixes

* address final review minor items ([a3c9145](https://github.com/Kehl-io/nestweaver/commit/a3c91450365c4cd93883d8ce0a43fc06bd1e82c1))
* address review findings for production readiness ([48c3171](https://github.com/Kehl-io/nestweaver/commit/48c31716c6c886a99730cb5b4260af3f7d882128))
* **snapshot:** query actual embedding dimension from store ([f72b923](https://github.com/Kehl-io/nestweaver/commit/f72b923b1993e9f48bea04bb52be2549b24d59ba))

## [0.15.0](https://github.com/Kehl-io/nestweaver/compare/v0.14.0...v0.15.0) (2026-06-02)


### Features

* **cli:** implement snapshot build command ([04d2085](https://github.com/Kehl-io/nestweaver/commit/04d20856b076c5f7e59850e0e9f620fc29ec0cd7))
* **cli:** implement snapshot push command ([52930c0](https://github.com/Kehl-io/nestweaver/commit/52930c046eabdabe1fd9a54701cf38deff530d26))
* **engine:** implement memory consolidate --apply ([24fda7a](https://github.com/Kehl-io/nestweaver/commit/24fda7a72dda6d4b4211618fdc9329e9f76182e7))
* **engine:** rewrite wikilinks after consolidate --apply moves files ([228f95f](https://github.com/Kehl-io/nestweaver/commit/228f95f7f24e158bbe524d8fc81cded29832ebf0))
* implement stubbed snapshot and consolidate features ([fff4504](https://github.com/Kehl-io/nestweaver/commit/fff4504ebb3058490c95382cf3252650fd7b929c))
* **snapshot:** switch to per-file checksums covering sidecars ([c385735](https://github.com/Kehl-io/nestweaver/commit/c385735a86aacee1b1cccfec4ef517117aa8706f))


### Bug Fixes

* address code review findings ([2b25c5a](https://github.com/Kehl-io/nestweaver/commit/2b25c5a827b0be214fe9ea8b27113eb4dc4dbcfd))
* apply rustfmt and clippy fixes ([308e2a7](https://github.com/Kehl-io/nestweaver/commit/308e2a70d6aafb7de998f9e34b61375380fb4706))
* **cli:** verify snapshot integrity after instance pull ([518317d](https://github.com/Kehl-io/nestweaver/commit/518317dccab6a9a0c78997727f991fb40b66451e))
* **snapshot:** decouple min_compatible_engine from build version ([8fe7b8a](https://github.com/Kehl-io/nestweaver/commit/8fe7b8a5f620f33b392b8dd0c2b621d86186b267))
* **storage:** LocalBackend push/pull now copies subdirectories ([af9c0a1](https://github.com/Kehl-io/nestweaver/commit/af9c0a111ceaba4f6c5702b7db89f5556ddf7a08))

## [0.14.0](https://github.com/Kehl-io/nestweaver/compare/v0.13.0...v0.14.0) (2026-06-02)


### Features

* AST-based type extraction via tree-sitter queries (zero re-parse cost) ([a47cf37](https://github.com/Kehl-io/nestweaver/commit/a47cf371d03f43a0d2ef265917a091dad1c1de81))
* **parser:** add tree-sitter type query files for 5 languages ([8e2c0c1](https://github.com/Kehl-io/nestweaver/commit/8e2c0c18735007a628876b192627093bc8dcacd6))
* **parser:** walk tree-sitter AST for type bindings (zero re-parse cost) ([076bc30](https://github.com/Kehl-io/nestweaver/commit/076bc30bbbd9b4c789c6dbf41b4ad6cdd7b52b56))
* **resolver:** feed AST type bindings into TypeEnvironment as primary source ([3c431e4](https://github.com/Kehl-io/nestweaver/commit/3c431e4dfc670dba8129bab3931b349543a129bd))


### Bug Fixes

* collapse nested if-let per clippy::collapsible_if ([57bc87a](https://github.com/Kehl-io/nestweaver/commit/57bc87a5d9289988b59189c4d8763a08ec1e322a))
* critical bugs batch 2 — daemon fresh-DB, skill generation, UTF-8 panic, write-routing ([c7ecc09](https://github.com/Kehl-io/nestweaver/commit/c7ecc0950d339382fd04cf3d50fa7504971865d9))
* critical bugs batch 2 — daemon fresh-DB, skill tool count, daemon write-routing ([b17f4f3](https://github.com/Kehl-io/nestweaver/commit/b17f4f3ab8e255ab3a519817cc674ee77a0a53b2))
* rustfmt formatting ([e4a9199](https://github.com/Kehl-io/nestweaver/commit/e4a9199fc071663ccd533a0529b8ac1974fc4469))
* UTF-8 char-boundary panic in type extractors + daemon fresh-DB canonicalize ([f1f798b](https://github.com/Kehl-io/nestweaver/commit/f1f798b0c63f0e0303ee5a1d7a07c76236b98a94))

## [0.13.0](https://github.com/Kehl-io/nestweaver/compare/v0.12.1...v0.13.0) (2026-06-01)


### Features

* **blast_radius:** confidence-weighted edge traversal for impact analysis ([dca11a8](https://github.com/Kehl-io/nestweaver/commit/dca11a8c781529fb2020f4a4be84bde29e9939da))
* **daemon:** set process title to nestweaver-daemon-{id} for pgrep ([4f1dc4c](https://github.com/Kehl-io/nestweaver/commit/4f1dc4c3a8d80e80311b5546cb0cf940616ae9e1))
* **engine:** co-change mining from git history with Jaccard scoring ([2c3286b](https://github.com/Kehl-io/nestweaver/commit/2c3286bae2dde22fb5840fa9ba297ebfbae5e6aa))
* **index:** build TypeEnvironments per file during indexing (not yet used for resolution) ([bb59ca6](https://github.com/Kehl-io/nestweaver/commit/bb59ca6159aa53e244d17d1aac4439a629930dc3))
* **parser:** extract parent class/struct name for method symbols ([72e94e3](https://github.com/Kehl-io/nestweaver/commit/72e94e304580f7e394c7e3299f60e80f084f4a33))
* **parser:** extract receiver from method call expressions ([53fb5d2](https://github.com/Kehl-io/nestweaver/commit/53fb5d2b9980b373f935439c1ef70e6d08c0fa6b))
* **resolver:** add per-language type extractors for annotations and constructors ([bb3fba4](https://github.com/Kehl-io/nestweaver/commit/bb3fba4a1ecd0a6a7c6473eae33c334a4f36f078))
* **resolver:** cross-file return type propagation for type-aware resolution ([d2fd4bf](https://github.com/Kehl-io/nestweaver/commit/d2fd4bf86e78a23b071dcf054f5c5e52c5492941))
* **resolver:** MRO walk for inherited methods in type-aware resolution ([9b03f97](https://github.com/Kehl-io/nestweaver/commit/9b03f970f3eff541cf3827da1267504015ca37b2))
* **resolver:** type-aware member call resolution using TypeEnvironment ([1db986d](https://github.com/Kehl-io/nestweaver/commit/1db986d350cbb23d4c789d4719f52c20e88b9d95))
* **resolver:** TypeEnvironment with 4-tier inference and fixpoint propagation ([d0f8136](https://github.com/Kehl-io/nestweaver/commit/d0f8136d04368dacff82b2e8609a09d47e3d62cb))
* type-aware call resolution, confidence-weighted impact analysis, co-change mining ([8c82ba6](https://github.com/Kehl-io/nestweaver/commit/8c82ba62eab6d0f955f6a64de05538c4ec56a290))


### Bug Fixes

* **cli:** allow --db after daemon subcommand with global arg ([0549139](https://github.com/Kehl-io/nestweaver/commit/05491397ded1fb8ea767ad2c284fdecb7b864962))
* **daemon:** shorten socket path to fit macOS 104-byte sun_path limit ([528302a](https://github.com/Kehl-io/nestweaver/commit/528302a8894753faa691965537ab5e03d972524b))
* derive tool documentation from registry instead of hardcoded tables ([99f43b2](https://github.com/Kehl-io/nestweaver/commit/99f43b2d771448f463a5977afc26da09a52e2082))
* resolve all clippy warnings in resolver crate ([c9f57d5](https://github.com/Kehl-io/nestweaver/commit/c9f57d581d2bf4a542308942efbe2d64e9b62135))
* **setup:** protect existing skill/rule files from overwrite ([9675d75](https://github.com/Kehl-io/nestweaver/commit/9675d75cf7df05c599183233a25d628f0d63fd9a))
* use multiplicative confidence decay instead of linear (research-backed) ([9d38ecc](https://github.com/Kehl-io/nestweaver/commit/9d38ecc7be6914e41951a4038a66bc5dee8213a3))
* warn on deprecated --allow-mcp-add-sources and strip from existing configs ([4a405c1](https://github.com/Kehl-io/nestweaver/commit/4a405c10b5dd0200a73cda532d4d5f084ee0f1a1))

## [0.12.1](https://github.com/Kehl-io/nestweaver/compare/v0.12.0...v0.12.1) (2026-06-01)


### Bug Fixes

* **ci:** add protobuf-compiler to release build workflow ([534a2f3](https://github.com/Kehl-io/nestweaver/commit/534a2f333817a10dd43b26f67c7063dc66166a32))

## [0.12.0](https://github.com/Kehl-io/nestweaver/compare/v0.11.0...v0.12.0) (2026-06-01)


### Features

* add Codex and JetBrains integrations, fix Windsurf config path ([4ed6d10](https://github.com/Kehl-io/nestweaver/commit/4ed6d107ff8379ab0ef4d9357c7c7cc0f64f4da9))
* add npm package for binary distribution (@kehl-io/nestweaver) ([f461c82](https://github.com/Kehl-io/nestweaver/commit/f461c820a55afce4dcb31ff4eb78612f28a85e02))
* agent guidance — hard rules in guides + subagent hook (F14, F15) ([0f1e9a2](https://github.com/Kehl-io/nestweaver/commit/0f1e9a2de2106bba5a3d81852f7b81cca3879f8a))
* agent interaction memory — PPR bias from usage patterns ([3e568f6](https://github.com/Kehl-io/nestweaver/commit/3e568f638c09f6f2133731820bcd2048e1179e9b))
* **algorithms:** add impact analysis BFS and substring search ([54164e0](https://github.com/Kehl-io/nestweaver/commit/54164e07e9fb9ce79345dedbd6ac6dd1ae2c9a40))
* **algorithms:** create nestweaver-algorithms crate with InMemoryGraph and PPR ([d3d2941](https://github.com/Kehl-io/nestweaver/commit/d3d294134ffa6ea0fa770c2d08562070267a6836))
* API contract graph — Contract nodes, IMPLEMENTS edges, drift (F2-core) ([70fd961](https://github.com/Kehl-io/nestweaver/commit/70fd9616929d5e124d0f95d82d5e681f4898518b))
* brain.* document-graph tools (F9) ([f095275](https://github.com/Kehl-io/nestweaver/commit/f0952759762f559836708d1e82c3b24db681be42))
* **branding:** add fierce geometric raptor logo with full asset suite ([d16ebea](https://github.com/Kehl-io/nestweaver/commit/d16ebea9271a4d561c279f6aee451e4982036c1f))
* **cli:** add --allow-writes flag to setup command ([62d06e2](https://github.com/Kehl-io/nestweaver/commit/62d06e2587470d5ce7d7b2adfee1cfecdb60f9af))
* **cli:** add --daemon flag to route index through gRPC daemon ([96e707d](https://github.com/Kehl-io/nestweaver/commit/96e707d9fc44b59940096366317ddb3c0245b4a7))
* **cli:** add --name flag for repo display name override ([52e1c05](https://github.com/Kehl-io/nestweaver/commit/52e1c05a6a20a1d529e27301e8ff2eb44d08be48))
* **cli:** add --stats flag and output control flags ([52449fb](https://github.com/Kehl-io/nestweaver/commit/52449fb46b96360e7a7cab9b97dff43c5a9f2f25))
* **cli:** add --tools flag for MCP tool allowlisting ([9d3c419](https://github.com/Kehl-io/nestweaver/commit/9d3c4191e7a7ae42065230a2e64be43e79907be3))
* **cli:** add daemon start/stop/status subcommands ([b567702](https://github.com/Kehl-io/nestweaver/commit/b567702a31be4156d8554e269692239e7df6cf77))
* **cli:** add nestweaver setup command for auto-configuring AI coding tools ([a158bc5](https://github.com/Kehl-io/nestweaver/commit/a158bc510d21d602e113d9d0e1d927ca927284ee))
* **cli:** add shell completions, miette diagnostics, and zero-config index ([e483aaf](https://github.com/Kehl-io/nestweaver/commit/e483aaf735dad9875ce77b7e5da167671deed0d3))
* **cli:** add standard output control flags ([60b218a](https://github.com/Kehl-io/nestweaver/commit/60b218ad46ce414934836aad8b9c52bd78939dfd))
* **client:** add nestweaver-client crate with auto-start and version check ([43560a4](https://github.com/Kehl-io/nestweaver/commit/43560a4c554a333c3821e332ca41eb3df7604eaa))
* **cli:** expose materialize-projects and detect-implicit-projects as subcommands ([1c03b29](https://github.com/Kehl-io/nestweaver/commit/1c03b29fe0aea75c62637cd3e8a33e56b5a6378f))
* **cli:** restore materialize-projects and detect-implicit-projects subcommands ([a86c787](https://github.com/Kehl-io/nestweaver/commit/a86c7877e5f211e4884bf251e879e578fde089e0))
* **cli:** show declared projects from --config alongside materialized ones in list-projects ([f336f93](https://github.com/Kehl-io/nestweaver/commit/f336f9314027fe5e35a89a67c12dd814c8e81e57))
* daemon-based concurrent database access with performance optimizations ([a54c2a9](https://github.com/Kehl-io/nestweaver/commit/a54c2a9f082ab8bb5598f754c9fc51ac4c71827e))
* **daemon:** add nestweaver-daemon crate with gRPC server scaffold ([a608a36](https://github.com/Kehl-io/nestweaver/commit/a608a36d6429d5f8867a432675ac4163a86f037d))
* **daemon:** daily log rotation via tracing-appender with non-blocking writer ([c254458](https://github.com/Kehl-io/nestweaver/commit/c254458a2b52b49f087a96296686c0f48a85f951))
* **daemon:** implement IndexRepo and IndexVault streaming RPCs ([44940c2](https://github.com/Kehl-io/nestweaver/commit/44940c278dbb81c5f3ec1e88d22412b11069f6b8))
* default MCP and CLI to daemon mode, add --no-daemon escape hatch ([b3ff013](https://github.com/Kehl-io/nestweaver/commit/b3ff0138461f9105582400dca33d7b7de232fe89))
* **engine:** add .brainignore support for vault indexing exclusion patterns ([9effc18](https://github.com/Kehl-io/nestweaver/commit/9effc18f741d557520232232e72db31de4f32cfd))
* **engine:** add dead code detection via entry point reachability ([5f4221c](https://github.com/Kehl-io/nestweaver/commit/5f4221c81fa807f4a4ec6f6e8a321cf1a3e0b698))
* **engine:** add graph export to Cypher, GraphML, and Mermaid ([0a9e5f8](https://github.com/Kehl-io/nestweaver/commit/0a9e5f8e24b5b21795cb9a824f9f492a71267862))
* **engine:** add hierarchical code summaries for token-efficient retrieval ([9a26820](https://github.com/Kehl-io/nestweaver/commit/9a268209e2d31d424181646e33cf748bfac7e9b7))
* **engine:** add HTML-to-markdown conversion for wiki content ingestion ([28b071f](https://github.com/Kehl-io/nestweaver/commit/28b071f3f366925b44a18cc21790821228d9be43))
* **engine:** add hub and bridge node detection ([30de53d](https://github.com/Kehl-io/nestweaver/commit/30de53d2c9be7da84c8d15e6a1d8e14051ecaa87))
* **engine:** add InteractionTracker with event recording, consolidation, and decay ([175b54c](https://github.com/Kehl-io/nestweaver/commit/175b54c530826d9fb03c3f51de4012e3d7e153ef))
* **engine:** add MCP client module for calling external MCP servers ([360dde4](https://github.com/Kehl-io/nestweaver/commit/360dde49e29469ac612c3951ca43e40deec691b4))
* **engine:** add multi-format guide generation (skill, cursor-rule, agents-md) ([1e579a5](https://github.com/Kehl-io/nestweaver/commit/1e579a5b204a94a365f9cf8a8fb0e30038b9fe7b))
* **engine:** add PR blast radius analysis with risk scoring ([70e2b91](https://github.com/Kehl-io/nestweaver/commit/70e2b9119e14444f1fda2bfac0b4857dd6eab65e))
* **engine:** add progress bars to indexing pipeline ([e328690](https://github.com/Kehl-io/nestweaver/commit/e328690df1de77f1286d4db710ab0cc189fae4c6))
* **engine:** add ProjectConfig, WikiSourceConfig, and McpServerConfig to instance config ([ab5338a](https://github.com/Kehl-io/nestweaver/commit/ab5338a2797d7f36c152e82667d492d061992a8c))
* **engine:** add Projects section to generated codebase guide ([b106df2](https://github.com/Kehl-io/nestweaver/commit/b106df21fd0d5cef605ec578145cc16789c31626))
* **engine:** add read_symbols — symbol-window source reads (F5) ([2759894](https://github.com/Kehl-io/nestweaver/commit/2759894df248da9c99c46d70dc35d8167e635a87))
* **engine:** add setup support for 10 additional AI tools ([1daf7d2](https://github.com/Kehl-io/nestweaver/commit/1daf7d213395894953efab33cea22eea9748d2e7))
* **engine:** add watch mode for live code re-indexing ([13e2fba](https://github.com/Kehl-io/nestweaver/commit/13e2fba37bb6c93743c87f02992bd2dd537a1d07))
* **engine:** affected_tests — static RTS for PR test selection (F13) ([0965cbd](https://github.com/Kehl-io/nestweaver/commit/0965cbde7db2d7962642a7c03fed6cc853373622))
* **engine:** decompose wiki notes into headings and sections after ingestion ([7cdef14](https://github.com/Kehl-io/nestweaver/commit/7cdef148449d634fdfb5729cdb89e87a68c6290b))
* **engine:** expand brain_search queries with taxonomy aliases for better recall ([2eaa63b](https://github.com/Kehl-io/nestweaver/commit/2eaa63b9abc3a99399485b9362b2d9823558f434))
* **engine:** finish agent feedback loop — TerminalSuccess + interactions show (F1) ([cfd0120](https://github.com/Kehl-io/nestweaver/commit/cfd0120d8ffbda88faea783ee1954de69c13d1a8))
* **engine:** generate SKILL.md conforming to Agent Skills standard ([a4c2e38](https://github.com/Kehl-io/nestweaver/commit/a4c2e38798698cb0fce8830d0a4cc62ff524bcb4))
* **engine:** implement project materialization with explicit and implicit declaration ([2389472](https://github.com/Kehl-io/nestweaver/commit/2389472acc4348febe1f93c65952fbf02e7c3065))
* **engine:** ingest wiki sources via MCP client calls during project materialization ([38bc3b0](https://github.com/Kehl-io/nestweaver/commit/38bc3b0862a924b584a018cd554a1ce709c42836))
* **engine:** inline high-confidence result bodies (F8) ([6543f72](https://github.com/Kehl-io/nestweaver/commit/6543f720737eb8522f984f6dd7dd1fd06807f706))
* **engine:** investigate bundle primitive (F10) ([03f765e](https://github.com/Kehl-io/nestweaver/commit/03f765ec5233bf3ac8f93a1049330d184ddf6c92))
* **engine:** lightweight result reranker (F17) ([55bc2f8](https://github.com/Kehl-io/nestweaver/commit/55bc2f8b1bca17301a214bcbd0ccc6b692470a43))
* **engine:** per-path dampen/boost ranking priors (F6) ([c183679](https://github.com/Kehl-io/nestweaver/commit/c183679cba79011dba4517a7a784a9363b2fc278))
* **engine:** retrieval-quality eval harness (P0.3) ([be5955f](https://github.com/Kehl-io/nestweaver/commit/be5955f86f99fea79bbceaf1e1a3e41b42ccd966))
* **engine:** unify brain_search to return both vault notes and code symbols ([db07420](https://github.com/Kehl-io/nestweaver/commit/db07420e310ad86484531dc41e3bcfd9d10e954b))
* git-activity-dampened CodeRank (F12) ([61faf2d](https://github.com/Kehl-io/nestweaver/commit/61faf2d26e912d4c208082955ac4c35e3c34888a))
* **integrations:** add SessionStart hook and PreToolUse blast radius enrichment for Claude Code ([911d9b6](https://github.com/Kehl-io/nestweaver/commit/911d9b639d826441243f0d1f0c5eadbd5d7edc12))
* **mcp,cli:** add project_context tool, list-projects command, and project seed expansion ([ee01c63](https://github.com/Kehl-io/nestweaver/commit/ee01c63d06cf4236b1c5c7c5c88c41687f0aec3f))
* **mcp,cli:** add tags and exclude_tags filters to brain_context ([ea36168](https://github.com/Kehl-io/nestweaver/commit/ea361689d06eed2006a39d1fa3267770ebd9390e))
* **mcp:** add --lite mode exposing 6 core tools for tool-capped environments ([495e7b0](https://github.com/Kehl-io/nestweaver/commit/495e7b0b4aed1930f62640646229a052c6004006))
* **mcp:** add daemon proxy mode with --daemon flag ([a74e71c](https://github.com/Kehl-io/nestweaver/commit/a74e71c26224551ca5743f62b16609f763f4aca3))
* **mcp:** add intent to project_context, tool allowlist, and section title indexing ([36b238e](https://github.com/Kehl-io/nestweaver/commit/36b238e22d07b89b7f45f058d640792f315e90a4))
* **mcp:** add interaction telemetry hooks to MCP tool dispatch ([207b4cc](https://github.com/Kehl-io/nestweaver/commit/207b4cc66c8de2c26b3dffc4f731f2aeb2f62cce))
* **mcp:** improve tool descriptions and add response_format parameter ([d178db1](https://github.com/Kehl-io/nestweaver/commit/d178db1aed20d6d29684d2e7eb0a4f48977ce691))
* **mcp:** wire intent through brain_context and cache get_summary in sidecar ([626918b](https://github.com/Kehl-io/nestweaver/commit/626918b10e8347d15c179b8f3c9f3ae8d6254194))
* memory-bank semantics — typed edges, lint, consolidate, related (F11) ([8d35b37](https://github.com/Kehl-io/nestweaver/commit/8d35b37d552223d3a7a17f8b125c6134a7c4523f))
* next-gen R3F web UI, algorithms/WASM crates, and v0.9.1 retrieval quality + eval harness ([028d62f](https://github.com/Kehl-io/nestweaver/commit/028d62f0eb5de29ad112c6a309995087143ce604))
* **parser:** add JSX component edges and confidence-aware dead code BFS ([965e4c7](https://github.com/Kehl-io/nestweaver/commit/965e4c74bcef917e24a252b753b6e611263933fa))
* **parser:** add Julia, SQL, HCL, Fortran, and Pascal language support ([9638978](https://github.com/Kehl-io/nestweaver/commit/9638978fef35ca8797cbfb1fe3c5a63351ab80d2))
* **parser:** add Lua, Bash, Scala, and Elixir language support ([d3556c1](https://github.com/Kehl-io/nestweaver/commit/d3556c1cfec308d64c4f6d3772d8b209ebb50ae7))
* **parser:** add Vue, Svelte, Astro, and SystemVerilog language support ([97509e3](https://github.com/Kehl-io/nestweaver/commit/97509e37d72f48b7666079689d2a84ecb6851355))
* **parser:** add Zig, Objective-C, Groovy, and PowerShell language support ([d4472c5](https://github.com/Kehl-io/nestweaver/commit/d4472c584b81b84528eca76d54396b328cafb8ad))
* **parser:** enrich symbol extraction with constants, properties, types, and expanded queries ([201c27d](https://github.com/Kehl-io/nestweaver/commit/201c27d67b7a292f29e3fdbfc405f015518c2298))
* persisted graph_generation (P0.2) + ZSTD response cache (F16) ([283e352](https://github.com/Kehl-io/nestweaver/commit/283e352fa37b4f7aafc7d951ad989571f7174e0a))
* **proto:** add nestweaver-proto crate with daemon gRPC service definition ([b6d8c11](https://github.com/Kehl-io/nestweaver/commit/b6d8c1192be0c0730483efbb434edefe24091ea0))
* **proto:** typed protobuf messages for 6 hot-path RPCs ([3165186](https://github.com/Kehl-io/nestweaver/commit/316518643f80ad55519ae91f3b070c49b5faa01d))
* **resolver:** add file-level IMPORTS edges for every resolved import ([7ce74dc](https://github.com/Kehl-io/nestweaver/commit/7ce74dcfa7ee49296fc0554aedafd64963086fda))
* **resolver:** add monorepo workspace package and tsconfig path alias resolution ([2eff955](https://github.com/Kehl-io/nestweaver/commit/2eff955fe63bc7e1970f9f8bc1665de410a93a38))
* **resolver:** create IMPORTS edges from resolved import references ([5e65cf5](https://github.com/Kehl-io/nestweaver/commit/5e65cf5314617a465b8a2b451201a6f82c8ae4ec))
* **schema:** add Symbol.end_line spanning a symbol's full source range (P0.1) ([45abd76](https://github.com/Kehl-io/nestweaver/commit/45abd764fe51a95b32ec41591fe509e87119e840))
* **schema:** add USES and ACCESSES edge types with PPR weighting and workspace resolution ([8ab7c21](https://github.com/Kehl-io/nestweaver/commit/8ab7c21a57a00e2ad3870569b11d0f5efc212abb))
* **store:** add graph_generation counter and watcher shared-store support ([d9bc01d](https://github.com/Kehl-io/nestweaver/commit/d9bc01d8416aa6fdd348a302cd2d31be865aac55))
* **store:** add Project edge types and read/write methods ([12fd7e3](https://github.com/Kehl-io/nestweaver/commit/12fd7e367dffaec72b114f26a68302a933739cd3))
* **store:** add Project nodes and edges to unified PPR graph scope ([4b3966c](https://github.com/Kehl-io/nestweaver/commit/4b3966c2642f9044a597c60908e8ea09a9379bca))
* **store:** add upsert helpers for idempotent project materialization ([1c24c4f](https://github.com/Kehl-io/nestweaver/commit/1c24c4ffaaab90a1f4c733003c26985d72b574dd))
* **store:** BM25 pseudo-relevance feedback (PRF) (F7) ([a950cac](https://github.com/Kehl-io/nestweaver/commit/a950cac348c8678d34de899eb99b7a79109d6391))
* **store:** dynamic PPR with query-type-aware alpha and edge weighting ([55123a3](https://github.com/Kehl-io/nestweaver/commit/55123a3d062c3eb091a81fca716e300d14587877))
* **store:** trigram-accelerated regex_search + count_patterns (F3, F4) ([efaa376](https://github.com/Kehl-io/nestweaver/commit/efaa376ec7aad2bcda90af809ff6b86a7fe8761e))
* **vscode:** add CodeLens, [@nestweaver](https://github.com/nestweaver) chat participant, status bar, and graph enrichment ([fb4699b](https://github.com/Kehl-io/nestweaver/commit/fb4699b6ab3dd8caaadb42c35b9460cd261e0d7d))
* **wasm:** add nestweaver-wasm crate, msgpack export, and snapshot endpoint ([c94cd38](https://github.com/Kehl-io/nestweaver/commit/c94cd3842027181515026493cc6e14e6e1ae9447))
* **wasm:** build WASM binary and wire worker/bridge to real wasm-bindgen API ([afb466f](https://github.com/Kehl-io/nestweaver/commit/afb466fd2ed9b1ce860aa2b4d86b43c2d250bcf1))
* **web:** add --watch flag for live re-indexing and /api/v1/version endpoint ([e3d14be](https://github.com/Kehl-io/nestweaver/commit/e3d14be5505e17183c07c00e6d1c939a22e04b54))
* **web:** add d3-force-3d layout worker and useForceLayout hook ([0e4e72e](https://github.com/Kehl-io/nestweaver/commit/0e4e72e97e697e30df82a093676b8f22e4893686))
* **web:** add edge gradients, keyboard graph navigation, and node drag ([8c7e4a5](https://github.com/Kehl-io/nestweaver/commit/8c7e4a54c6950dd809c2753b45e619d1029b59d0))
* **web:** add edge particles, accessible node list view, and view mode toggle ([34d9c99](https://github.com/Kehl-io/nestweaver/commit/34d9c99ab160f24876b79a26f43341e540da5a5c))
* **web:** add glassmorphism, navigation history, URL deep-linking, reduced effects toggle ([4018d75](https://github.com/Kehl-io/nestweaver/commit/4018d75e838c4dbe7ab6b9d4e9456dd4331387b5))
* **web:** add node labels for seed, selected, and hovered nodes ([8df07a4](https://github.com/Kehl-io/nestweaver/commit/8df07a42810f409112be295d5bbe51abba2ef966))
* **web:** add Obsidian-style graph polish — animated settling, always-visible labels, click-to-focus ([76e28f5](https://github.com/Kehl-io/nestweaver/commit/76e28f544b92f54f7a2655b90491b5523399f2cd))
* **web:** add R3F packages and graphDataSlice + useGraphBridge data layer ([0b298cd](https://github.com/Kehl-io/nestweaver/commit/0b298cd8feed3dc287ea45519eb069c2c3dcb8cc))
* **web:** add R3F renderer core — GraphCanvas, NodeInstanceMesh, EdgeInstanceMesh, GPU picking ([59403d3](https://github.com/Kehl-io/nestweaver/commit/59403d31a01f1195c91ab674c9145e1581e9d5a8))
* **web:** add visual effects — breathing, glow, bloom, hover interactions ([464e079](https://github.com/Kehl-io/nestweaver/commit/464e07917011ae69e5a4be92e6baeaa0404a7940))
* **web:** add WASM engine bridge, snapshot sync, and engine mode toggle ([7906319](https://github.com/Kehl-io/nestweaver/commit/790631988cdee53b6c39b3c8dd50902937ce6769))
* **web:** implement SemanticZoom camera bridge and CommunityOverlay with 3D hulls ([48141bb](https://github.com/Kehl-io/nestweaver/commit/48141bbc5e3aeb700a5244cc5b9ef1ff63555b25))
* **web:** migrate auxiliary components and remove Sigma.js dependency ([a9afa1c](https://github.com/Kehl-io/nestweaver/commit/a9afa1c8fc0afba22fe862a243dca97edbacd0cb))
* **web:** migrate GraphPanel and mode hooks from Sigma.js to R3F ([18fcc85](https://github.com/Kehl-io/nestweaver/commit/18fcc85c6a4226085710ea4d7332d96a71ddd134))
* **web:** wire GlassPanel into panels and add impact ripple on selection ([b755127](https://github.com/Kehl-io/nestweaver/commit/b75512781b3a634a4c4dbf717da5a58fb83a3746))
* **web:** wire WASM engine end-to-end, fix navigation history, fix GlassPanel cursor light ([cec096e](https://github.com/Kehl-io/nestweaver/commit/cec096eebe4622f5cfa3b634aa5412f162fcc374))


### Bug Fixes

* --force re-index idempotency + broken-links surfaces unresolved wikilinks (QA) ([bec3d12](https://github.com/Kehl-io/nestweaver/commit/bec3d128e35c52507ec509de9897e95521b1abf6))
* add --config to read-only commands, EdgeType PPR classification, parallel parsing with rayon ([3c3392d](https://github.com/Kehl-io/nestweaver/commit/3c3392dd371bcba63f20ea1c0655cbd3915cd102))
* add generation field to SSE events and update GPU picking docs ([fc26c87](https://github.com/Kehl-io/nestweaver/commit/fc26c87995670c934aa9d0c8ee09af4bfbd19f10))
* add missing MCP schema params, fix timezone parsing, extract shared recency utils ([3ba9be5](https://github.com/Kehl-io/nestweaver/commit/3ba9be5a84e451d7958199324b700c1e74d6a074))
* address all code review low and nit findings ([03684dd](https://github.com/Kehl-io/nestweaver/commit/03684dd457b3d84ebf86d7618d84a48fa9ddef2e))
* address all remaining code review findings (14 fixes) ([a6e6777](https://github.com/Kehl-io/nestweaver/commit/a6e6777f0f1e262003667b5f0236e33d40bed6b6))
* address code review findings (12 fixes) ([b8c2e85](https://github.com/Kehl-io/nestweaver/commit/b8c2e857f1f22f9eb895716ae59e6000a694e150))
* address review findings (hook command, install URLs, JSON safety, CodeLens caching) ([428f2c3](https://github.com/Kehl-io/nestweaver/commit/428f2c3ef56ce9a8b0ca953089c908088b04d661))
* affected-tests reaches Jest/Vitest tests + consistent not-found exit codes (QA) ([dd0d48c](https://github.com/Kehl-io/nestweaver/commit/dd0d48cb4b545dbf42d996520be694b24c2ba122))
* auto-populate Tantivy after brain add/refresh, fix remaining user issues ([7ab2872](https://github.com/Kehl-io/nestweaver/commit/7ab2872152a89806477a6641936a2a36745e410f))
* auto-populate Tantivy after brain add/refresh, implement wiki refresh timer, fix WAL flush ([2f1c168](https://github.com/Kehl-io/nestweaver/commit/2f1c16857103c707bb69980531e402621d14ff95))
* **bench:** add intent parameter to brain benchmark ([bbd2d18](https://github.com/Kehl-io/nestweaver/commit/bbd2d185eaa9b77cdb19a13170565c4de4c3518b))
* brain_status last_indexed, clusters exit code + adaptive resolution, get_summary diagnostics ([8e9540b](https://github.com/Kehl-io/nestweaver/commit/8e9540ba4ace42e23464071b9502d46b993356dc))
* **ci:** add --allow-multiple-definition for aarch64-linux release builds ([00902cc](https://github.com/Kehl-io/nestweaver/commit/00902cc098df0bd9794e657822264422116cbffd))
* **ci:** add build deps and lbug cache warming to release build jobs ([b82ca97](https://github.com/Kehl-io/nestweaver/commit/b82ca97ca58f5e73e491e0d07cc82427d3140a1c))
* **ci:** add cargo build step before tests for lbug prebuilt download ([b189912](https://github.com/Kehl-io/nestweaver/commit/b189912338c1b714a565d5b5376269ee1769175a))
* **ci:** add linker config to clippy job and make audit advisory (non-blocking) ([d5ffaf2](https://github.com/Kehl-io/nestweaver/commit/d5ffaf2128275d9a67ad4bf2b61d846047244c06))
* **ci:** add protobuf-compiler to CI, fix rustfmt formatting ([7d0c617](https://github.com/Kehl-io/nestweaver/commit/7d0c617f60f484251438c82e5631afa0910a3838))
* **ci:** build nestweaver-store first to cache lbug prebuilt before full workspace build ([3e50674](https://github.com/Kehl-io/nestweaver/commit/3e50674853a51434a820e10959203097237cb200))
* **ci:** build test targets to ensure lbug library is available for test linking ([1173695](https://github.com/Kehl-io/nestweaver/commit/1173695706c331e3bb227e795673a22a4320a69a))
* **ci:** configure release-please for cargo workspace and update repo URLs to Kehl-io ([f6e7e3c](https://github.com/Kehl-io/nestweaver/commit/f6e7e3cf46a48ce12f41747c748c4909986bc9c6))
* **ci:** configure release-please to bump Cargo.toml workspace version automatically ([2d15106](https://github.com/Kehl-io/nestweaver/commit/2d15106fe3b9a52acaa7a59b3bb2ad803cd8d813))
* **ci:** install cmake and g++ for lbug native library build ([146be75](https://github.com/Kehl-io/nestweaver/commit/146be75e38dc5b2de2884f83d34352b78566f711))
* **ci:** install g++ cross-compiler for aarch64-linux release builds ([19ac794](https://github.com/Kehl-io/nestweaver/commit/19ac794767aff49e08a4d4129d059ee9ed7ee3db))
* **ci:** pre-download liblbug before build (LadybugDB recommended pattern) ([6eab138](https://github.com/Kehl-io/nestweaver/commit/6eab138352238cd35019c3e101d35e67037830b1))
* **ci:** resolve clippy lints and add NESTWEAVER_NO_DAEMON to CI test/coverage jobs ([a257b88](https://github.com/Kehl-io/nestweaver/commit/a257b8880cdeac6989c7224795a719b94464f3e2))
* **ci:** retry build after lbug prebuilt download race condition ([a0752d7](https://github.com/Kehl-io/nestweaver/commit/a0752d7c5759da67bfe5af7b107c5bef71deaac9))
* **ci:** run cargo build before clippy to trigger lbug prebuilt download ([1e83655](https://github.com/Kehl-io/nestweaver/commit/1e83655a6a524f1fe625841e3531e28725d5ce85))
* **ci:** show full build output for lbug debugging ([a08dbae](https://github.com/Kehl-io/nestweaver/commit/a08dbae8d5441a940308c0cd413afa41c284448f))
* **ci:** simplify release-please config and document required repo permissions ([1c8ba16](https://github.com/Kehl-io/nestweaver/commit/1c8ba166542f099131660644bb2173472e8098aa))
* **ci:** switch release-please to simple type for workspace.package.version compatibility ([30461cc](https://github.com/Kehl-io/nestweaver/commit/30461cc10d2f047f65bfbb6bfbd7ce4ddc65290d))
* **ci:** use LBUG_BUILD_FROM_SOURCE for release builds (reliable cross-platform) ([2bbf192](https://github.com/Kehl-io/nestweaver/commit/2bbf192af945d507cb2feedfbfd5c38deba1a424))
* **ci:** warm lbug cache for target architecture in release builds ([0c51629](https://github.com/Kehl-io/nestweaver/commit/0c51629063377141b06c6c1ebffd5de6f2c9a565))
* CLI alias resolver, SIGTERM handling, clusters exit code, brain status timestamps ([fb036eb](https://github.com/Kehl-io/nestweaver/commit/fb036eb340cce95d57978829b02a41176ebe4eca))
* **cli:** auto-wire manifests path in brain watch for manifest auto-refresh ([93a94b1](https://github.com/Kehl-io/nestweaver/commit/93a94b112bb3db69d4e16abb7a0682438f8c75fa))
* **cli:** collapse nested if to satisfy clippy::collapsible_if ([b59c3b4](https://github.com/Kehl-io/nestweaver/commit/b59c3b4252b4129c95b9588b4495b725fe0b416f))
* **client:** correct daemon spawn arg order and increase socket timeout to 5s ([745b46a](https://github.com/Kehl-io/nestweaver/commit/745b46a458a79b7d10302acbe2b256e75443bb4c))
* **client:** fix tonic UDS client connection for 0.13 ([6855634](https://github.com/Kehl-io/nestweaver/commit/685563433f8558e54fe3a34661980daa713901a9))
* **cli:** match --repo filter against repo display name, not just file path ([5db4215](https://github.com/Kehl-io/nestweaver/commit/5db421503d2e73c97481af12d4984fa75f2d408d))
* **cli:** restore --allow-writes flag on setup command ([d08e936](https://github.com/Kehl-io/nestweaver/commit/d08e9367445c6072c4bb6b2e997bdc5063a41e81))
* **cli:** thread --token-budget through project-context command ([d6a2136](https://github.com/Kehl-io/nestweaver/commit/d6a2136d492f3c0d6285396377e397bb0289d9d1))
* **cli:** write Claude Code MCP config to .mcp.json instead of .claude/settings.json ([2cc75cd](https://github.com/Kehl-io/nestweaver/commit/2cc75cd755eada04b9f0dcd2342918c576a92773))
* **daemon:** catch SIGTERM for graceful shutdown with socket/pidfile cleanup ([fb621a2](https://github.com/Kehl-io/nestweaver/commit/fb621a27ba75f02445dd9cbd6eb6c727ac9661c5))
* **daemon:** graceful shutdown via signal channel, open DB with write access, spawn_blocking for dispatch ([ccaf96c](https://github.com/Kehl-io/nestweaver/commit/ccaf96c86d1f8d0ebffb8c78e131555055d26683))
* **daemon:** use _with_store index variants to avoid DB lock re-acquisition ([1754e93](https://github.com/Kehl-io/nestweaver/commit/1754e9345fe93099efd437bf2b588d094826652e))
* **engine:** add timeout and EOF detection to MCP client to prevent wiki_sources hang ([63efb37](https://github.com/Kehl-io/nestweaver/commit/63efb372db8fe6e8dfe2249362b42e28a56c4a5b))
* **engine:** align sidecar paths between snapshot and index for pagerank/manifests ([a39de32](https://github.com/Kehl-io/nestweaver/commit/a39de32f3fba2bc01f7378844c1f4266bfce8922))
* **engine:** boost project-scoped content in PPR ranking for project_context queries ([5bcebeb](https://github.com/Kehl-io/nestweaver/commit/5bcebebabcf0e6e6a420262ee4dd5380bf0281aa))
* **engine:** clean existing project edges before re-materialization ([7516162](https://github.com/Kehl-io/nestweaver/commit/7516162e5676824a5037be40da678095344bcc55))
* **engine:** correct interaction memory implementation per research alignment ([a4b4985](https://github.com/Kehl-io/nestweaver/commit/a4b4985698089c071e2b68a36264d38becd2849a))
* **engine:** fix McpClient BufReader recreation and add notification skipping ([bde2ec3](https://github.com/Kehl-io/nestweaver/commit/bde2ec316701b8b3d18796f770622dcd0fadfc28))
* **engine:** improve dead code detection accuracy with IMPORTS edge traversal and entry point coverage ([b7bd060](https://github.com/Kehl-io/nestweaver/commit/b7bd060b512fcbc4c51c8ab78ccf1daca62ef6c4))
* **engine:** limit cluster summaries to top-50 largest, skip singletons ([0a7f777](https://github.com/Kehl-io/nestweaver/commit/0a7f777dc2e866353e561c9d0ab65c656375e5b9))
* **engine:** reduce dead code false positives with type exclusion, dedup, and manifest entry points ([a2f5431](https://github.com/Kehl-io/nestweaver/commit/a2f5431701b250de1d071ba80160a3c0a8c82539))
* **engine:** robust MCP client with timeout, poisoning, reuse, and configurable timeout_secs ([18c04e8](https://github.com/Kehl-io/nestweaver/commit/18c04e8cdaf23b731039facc6a9f94975d73c778))
* **engine:** use atomic writes for extension store sidecar ([0acfcd8](https://github.com/Kehl-io/nestweaver/commit/0acfcd8759b2d300f16092533f9619196c5d8bae))
* **engine:** wire project alias resolution through extension sidecar ([ae8b7f4](https://github.com/Kehl-io/nestweaver/commit/ae8b7f468f1286156ea9c45fe693670172a665f8))
* **grpc:** increase message size limit to 64MB for large responses (dead_code, clusters) ([3504d87](https://github.com/Kehl-io/nestweaver/commit/3504d87faed9e0df1472e3f7dcf42ab54e2d6cdf))
* hash-based instance ID to prevent socket path collisions and length overflow ([9639ee0](https://github.com/Kehl-io/nestweaver/commit/9639ee0dd9c89b62f2c64d8822f814a6db06df8e))
* hide deprecated --allow-writes flag from setup CLI help ([8c57598](https://github.com/Kehl-io/nestweaver/commit/8c5759818eb1f1d7d194b05230fb5f9882b9b4f7))
* **mcp,cli:** wire alias sidecar into brain_context for automatic seed resolution ([4273cf9](https://github.com/Kehl-io/nestweaver/commit/4273cf9f6061ae310a22db8e8268961196e2c354))
* **mcp:** align client protocol version with server ([859b11f](https://github.com/Kehl-io/nestweaver/commit/859b11ffdff216f6d07210a690cbd81a6cb99e91))
* **mcp:** correct project UID lookup in project_context tool ([93a8192](https://github.com/Kehl-io/nestweaver/commit/93a8192c8def0b91e3ab131fcb5a069b443005da))
* **mcp:** resolve ambiguous symbol names and repo display-name filter ([47196c1](https://github.com/Kehl-io/nestweaver/commit/47196c1257007b3bdfe33baa12c9bd39a5712a0f))
* **mcp:** resolve vault display names in vaults filter ([de25f50](https://github.com/Kehl-io/nestweaver/commit/de25f50699441a189ad74c3f1d01ae73e81df545))
* **mcp:** strip seeds array from default response to respect token budget ([6caf8f8](https://github.com/Kehl-io/nestweaver/commit/6caf8f885c705b9decc09e1a4511157719905387))
* **mcp:** tighten token budget with serialization-based final pass and relevance cost ([0f207b8](https://github.com/Kehl-io/nestweaver/commit/0f207b8e7c50e5880bab554d8b1e87ea82222fef))
* **mcp:** update tool count assertion for get_summary tool ([ede630a](https://github.com/Kehl-io/nestweaver/commit/ede630a48988aba66f22e028b9e111d13c331200))
* **mcp:** use ProjectContext intent as default for project_context tool ([65babbe](https://github.com/Kehl-io/nestweaver/commit/65babbef482852914b7b59cf1b7764436a66718f))
* **mcp:** validate hybrid search weights and clamp to non-negative ([62a3ab2](https://github.com/Kehl-io/nestweaver/commit/62a3ab2732cf501030d6731ef7476ccedccba0b5))
* **parser:** improve entry point detection for React, Next.js, and frontend frameworks ([ef9dd13](https://github.com/Kehl-io/nestweaver/commit/ef9dd13f9a775dd126ccc513b66803bcd1f35428))
* **parser:** use TSX grammar for .tsx files to fix JSX parse failures ([042f6ee](https://github.com/Kehl-io/nestweaver/commit/042f6ee5d9e18722dec009bbe1627726f5245a59))
* regex line numbers + install-hook dry-run delta + multi-handler coverage (QA) ([c34bc1d](https://github.com/Kehl-io/nestweaver/commit/c34bc1d0737ecaa85ff8d36a0e2119110aaac465))
* remove duplicate PID write, fix brain_add_source for plain markdown dirs, implement daemon restart ([587698d](https://github.com/Kehl-io/nestweaver/commit/587698def37bd90951e600fc6c173c5a1631a573))
* remove stale --allow-mcp-add-sources references from tool descriptions and setup configs ([1038439](https://github.com/Kehl-io/nestweaver/commit/10384397515519eec03c0429d2066d42f0be3d2e))
* remove unused variables in autostart error path ([b36c0be](https://github.com/Kehl-io/nestweaver/commit/b36c0bea6cd13266f603072789481c518d5422cb))
* repo_display_name in materialize, --db on list-links, positional watch, --repo filter on impact ([efbaeee](https://github.com/Kehl-io/nestweaver/commit/efbaeeec9b3dcb660b22a14a0fea273b210bbcca))
* resolve db from --config on brain read commands (Bug [#19](https://github.com/Kehl-io/nestweaver/issues/19)) ([e25c00e](https://github.com/Kehl-io/nestweaver/commit/e25c00e9c53d08c5561485dd49cb275bb4d47613))
* restore project-context results, reject wiki fetch errors, restore CLI subcommands ([47031e1](https://github.com/Kehl-io/nestweaver/commit/47031e1c3328356c083273251acc791cae4e5b45))
* restore render_cost, add --config to all commands, bump version to 0.9.0 ([4ce7f3b](https://github.com/Kehl-io/nestweaver/commit/4ce7f3bd77f950b4252b8ab287b8c9873c54478e))
* **schema:** add ProjectIncludesNote to EdgeType enum for completeness ([4b702b4](https://github.com/Kehl-io/nestweaver/commit/4b702b4f320ee57f4fecdcfb026275107a22af25))
* **store:** add missing PROJECT rel tables and confidence column to schema ([fc2c55d](https://github.com/Kehl-io/nestweaver/commit/fc2c55ddd1865fd9ea6c3fd46a2b3c7dfd9069bf))
* **store:** add missing USES and ACCESSES rel tables to schema bootstrap ([6ca9bbf](https://github.com/Kehl-io/nestweaver/commit/6ca9bbf4da0cbe1e55e16fabfec6aaff6545fad7))
* **store:** add read-only Tantivy mode to prevent writer lock blocking searches ([1ecb7ec](https://github.com/Kehl-io/nestweaver/commit/1ecb7ec449ec15afd5497998bb0fcd5e470ac8db))
* **store:** deduplicate project nodes by name on upsert, correct MCP token budget measurement ([340a143](https://github.com/Kehl-io/nestweaver/commit/340a143bc77a6472244e464e416ca566bc1a4397))
* **store:** run schema migrations on open() for v0.7 DB compatibility ([fb5e300](https://github.com/Kehl-io/nestweaver/commit/fb5e3004942c248306212bed7572b4b13e9fab3f))
* **store:** use asymmetric edge weighting in PPR (0.3x for reverse edges) ([5d783d0](https://github.com/Kehl-io/nestweaver/commit/5d783d0deeeccd941071f6cba4ae063f19335a4b))
* **store:** use parameterized queries for project edge lookups ([aa7beba](https://github.com/Kehl-io/nestweaver/commit/aa7beba76ec513fdc016c0a04af51243acb62a3d))
* **store:** use SYMBOL_COLUMNS in update_symbol_embedding query ([607d0fa](https://github.com/Kehl-io/nestweaver/commit/607d0fabd6958e62e805afe52ff33eca6a41237a))
* surface project member notes in project_context (Bug [#12](https://github.com/Kehl-io/nestweaver/issues/12)) ([38c9414](https://github.com/Kehl-io/nestweaver/commit/38c941408eed4ceaa011613abddffb86ed9a6876))
* **ui:** truncate labels at overview zoom instead of hiding them ([41d2db1](https://github.com/Kehl-io/nestweaver/commit/41d2db10b60c3cb4a3eadfdfbb3e0924c5b51ab8))
* update stale daemonize references to daemonize2 in comments ([ebb6bee](https://github.com/Kehl-io/nestweaver/commit/ebb6bee738bb21bf69c31579e6e513ad46662bfc))
* watcher lock detection, daemon log rotation, actionable error messages ([6f0fd50](https://github.com/Kehl-io/nestweaver/commit/6f0fd503e7e46f1a511aa482eca1c8ae0db462a2))
* **watcher:** remove stale title→UID mapping on note rename before adding new title ([0c86af1](https://github.com/Kehl-io/nestweaver/commit/0c86af1488c6333341d332c553a7e813c9358e24))
* **web:** fix E2E test assertions and CI build order for frontend embedding ([a2f2269](https://github.com/Kehl-io/nestweaver/commit/a2f22699f6e09be9d0bb481b8be522a0bb187c61))
* **web:** fix edge rendering with depthTest disabled for visibility ([d4bc6d9](https://github.com/Kehl-io/nestweaver/commit/d4bc6d900ea6bcb47845a2d66cef4f33184adc37))
* **web:** replace LineSegments with instanced quads for visible edge rendering ([f036a6a](https://github.com/Kehl-io/nestweaver/commit/f036a6a61437b014500017d77de7e15d83eb91fb))
* **web:** resolve TypeScript compilation errors ([bf67137](https://github.com/Kehl-io/nestweaver/commit/bf67137c99404d4448c1e2b410359e839bae29c9))
* **web:** run force layout to completion before rendering — no more flying nodes ([6cc0780](https://github.com/Kehl-io/nestweaver/commit/6cc07803039a00d6fe85a0aa5660a45817da67a5))
* **web:** strip Symbol/ prefix from kind in color lookup ([c488c28](https://github.com/Kehl-io/nestweaver/commit/c488c28281f1ffda15fa94c6e90a4874eb55503d))
* **web:** transparent dark logo background and theme-aware TopBar ([92dfff5](https://github.com/Kehl-io/nestweaver/commit/92dfff514d7ba4dbf39a2dcf968a356166972b6f))


### Performance Improvements

* **daemon:** skip Tantivy reindex after code repo indexing (Tantivy only indexes notes) ([b375e40](https://github.com/Kehl-io/nestweaver/commit/b375e4071d0f52fac34efe11a1e858c7312b604a))
* **engine:** tiered change detection for faster incremental indexing ([ca0f4f4](https://github.com/Kehl-io/nestweaver/commit/ca0f4f4d50ced1568b227e46e1c550da97a36273))
* **index:** defer PageRank computation to first query (lazy evaluation) ([b7b044c](https://github.com/Kehl-io/nestweaver/commit/b7b044c43f50dd351ed6e840b3ebfdab6e5c8323))
* **index:** parallelize markdown note parsing with rayon ([766f82e](https://github.com/Kehl-io/nestweaver/commit/766f82e5f82bfc76703a49db7857c69e2fc8f011))
* **pagerank:** add warm-start support for faster convergence after incremental updates ([c54a429](https://github.com/Kehl-io/nestweaver/commit/c54a4291e1d3d8de927200033e706a9226fb6713))
* **store:** bulk DETACH DELETE for file symbols instead of per-UID queries ([96b2fd3](https://github.com/Kehl-io/nestweaver/commit/96b2fd36d46d1336ea8d026ccb0dee296056ed53))
* **store:** wrap bulk index writes in transactions for 12x speedup ([0422b43](https://github.com/Kehl-io/nestweaver/commit/0422b43481c2ba3c82ba3518a01ab744d3275bcc))
* **tantivy:** update search index after daemon indexing operations ([da90564](https://github.com/Kehl-io/nestweaver/commit/da90564fa6a112629fb75d1e9815672dd7ad515f))
* **watcher:** bidirectional map for O(1) wikilink title lookup updates on rename ([e173612](https://github.com/Kehl-io/nestweaver/commit/e17361201a8cf9be0273affb96e0e14068e8a502))
* **watcher:** cache wikilink title lookup across batch, avoid per-note list_notes query ([5137d87](https://github.com/Kehl-io/nestweaver/commit/5137d871983375ba7f29bb60e2738aab19aace74))

## [0.11.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.10.0...nestweaver-v0.11.0) (2026-05-29)


### Features

* agent guidance — hard rules in guides + subagent hook (F14, F15) ([0f1e9a2](https://github.com/Kehl-io/nestweaver/commit/0f1e9a2de2106bba5a3d81852f7b81cca3879f8a))
* **algorithms:** add impact analysis BFS and substring search ([54164e0](https://github.com/Kehl-io/nestweaver/commit/54164e07e9fb9ce79345dedbd6ac6dd1ae2c9a40))
* **algorithms:** create nestweaver-algorithms crate with InMemoryGraph and PPR ([d3d2941](https://github.com/Kehl-io/nestweaver/commit/d3d294134ffa6ea0fa770c2d08562070267a6836))
* API contract graph — Contract nodes, IMPLEMENTS edges, drift (F2-core) ([70fd961](https://github.com/Kehl-io/nestweaver/commit/70fd9616929d5e124d0f95d82d5e681f4898518b))
* brain.* document-graph tools (F9) ([f095275](https://github.com/Kehl-io/nestweaver/commit/f0952759762f559836708d1e82c3b24db681be42))
* **engine:** add read_symbols — symbol-window source reads (F5) ([2759894](https://github.com/Kehl-io/nestweaver/commit/2759894df248da9c99c46d70dc35d8167e635a87))
* **engine:** affected_tests — static RTS for PR test selection (F13) ([0965cbd](https://github.com/Kehl-io/nestweaver/commit/0965cbde7db2d7962642a7c03fed6cc853373622))
* **engine:** finish agent feedback loop — TerminalSuccess + interactions show (F1) ([cfd0120](https://github.com/Kehl-io/nestweaver/commit/cfd0120d8ffbda88faea783ee1954de69c13d1a8))
* **engine:** inline high-confidence result bodies (F8) ([6543f72](https://github.com/Kehl-io/nestweaver/commit/6543f720737eb8522f984f6dd7dd1fd06807f706))
* **engine:** investigate bundle primitive (F10) ([03f765e](https://github.com/Kehl-io/nestweaver/commit/03f765ec5233bf3ac8f93a1049330d184ddf6c92))
* **engine:** lightweight result reranker (F17) ([55bc2f8](https://github.com/Kehl-io/nestweaver/commit/55bc2f8b1bca17301a214bcbd0ccc6b692470a43))
* **engine:** per-path dampen/boost ranking priors (F6) ([c183679](https://github.com/Kehl-io/nestweaver/commit/c183679cba79011dba4517a7a784a9363b2fc278))
* **engine:** retrieval-quality eval harness (P0.3) ([be5955f](https://github.com/Kehl-io/nestweaver/commit/be5955f86f99fea79bbceaf1e1a3e41b42ccd966))
* git-activity-dampened CodeRank (F12) ([61faf2d](https://github.com/Kehl-io/nestweaver/commit/61faf2d26e912d4c208082955ac4c35e3c34888a))
* memory-bank semantics — typed edges, lint, consolidate, related (F11) ([8d35b37](https://github.com/Kehl-io/nestweaver/commit/8d35b37d552223d3a7a17f8b125c6134a7c4523f))
* next-gen R3F web UI, algorithms/WASM crates, and v0.9.1 retrieval quality + eval harness ([028d62f](https://github.com/Kehl-io/nestweaver/commit/028d62f0eb5de29ad112c6a309995087143ce604))
* persisted graph_generation (P0.2) + ZSTD response cache (F16) ([283e352](https://github.com/Kehl-io/nestweaver/commit/283e352fa37b4f7aafc7d951ad989571f7174e0a))
* **schema:** add Symbol.end_line spanning a symbol's full source range (P0.1) ([45abd76](https://github.com/Kehl-io/nestweaver/commit/45abd764fe51a95b32ec41591fe509e87119e840))
* **store:** add graph_generation counter and watcher shared-store support ([d9bc01d](https://github.com/Kehl-io/nestweaver/commit/d9bc01d8416aa6fdd348a302cd2d31be865aac55))
* **store:** BM25 pseudo-relevance feedback (PRF) (F7) ([a950cac](https://github.com/Kehl-io/nestweaver/commit/a950cac348c8678d34de899eb99b7a79109d6391))
* **store:** trigram-accelerated regex_search + count_patterns (F3, F4) ([efaa376](https://github.com/Kehl-io/nestweaver/commit/efaa376ec7aad2bcda90af809ff6b86a7fe8761e))
* **wasm:** add nestweaver-wasm crate, msgpack export, and snapshot endpoint ([c94cd38](https://github.com/Kehl-io/nestweaver/commit/c94cd3842027181515026493cc6e14e6e1ae9447))
* **wasm:** build WASM binary and wire worker/bridge to real wasm-bindgen API ([afb466f](https://github.com/Kehl-io/nestweaver/commit/afb466fd2ed9b1ce860aa2b4d86b43c2d250bcf1))
* **web:** add --watch flag for live re-indexing and /api/v1/version endpoint ([e3d14be](https://github.com/Kehl-io/nestweaver/commit/e3d14be5505e17183c07c00e6d1c939a22e04b54))
* **web:** add d3-force-3d layout worker and useForceLayout hook ([0e4e72e](https://github.com/Kehl-io/nestweaver/commit/0e4e72e97e697e30df82a093676b8f22e4893686))
* **web:** add edge gradients, keyboard graph navigation, and node drag ([8c7e4a5](https://github.com/Kehl-io/nestweaver/commit/8c7e4a54c6950dd809c2753b45e619d1029b59d0))
* **web:** add edge particles, accessible node list view, and view mode toggle ([34d9c99](https://github.com/Kehl-io/nestweaver/commit/34d9c99ab160f24876b79a26f43341e540da5a5c))
* **web:** add glassmorphism, navigation history, URL deep-linking, reduced effects toggle ([4018d75](https://github.com/Kehl-io/nestweaver/commit/4018d75e838c4dbe7ab6b9d4e9456dd4331387b5))
* **web:** add node labels for seed, selected, and hovered nodes ([8df07a4](https://github.com/Kehl-io/nestweaver/commit/8df07a42810f409112be295d5bbe51abba2ef966))
* **web:** add Obsidian-style graph polish — animated settling, always-visible labels, click-to-focus ([76e28f5](https://github.com/Kehl-io/nestweaver/commit/76e28f544b92f54f7a2655b90491b5523399f2cd))
* **web:** add R3F packages and graphDataSlice + useGraphBridge data layer ([0b298cd](https://github.com/Kehl-io/nestweaver/commit/0b298cd8feed3dc287ea45519eb069c2c3dcb8cc))
* **web:** add R3F renderer core — GraphCanvas, NodeInstanceMesh, EdgeInstanceMesh, GPU picking ([59403d3](https://github.com/Kehl-io/nestweaver/commit/59403d31a01f1195c91ab674c9145e1581e9d5a8))
* **web:** add visual effects — breathing, glow, bloom, hover interactions ([464e079](https://github.com/Kehl-io/nestweaver/commit/464e07917011ae69e5a4be92e6baeaa0404a7940))
* **web:** add WASM engine bridge, snapshot sync, and engine mode toggle ([7906319](https://github.com/Kehl-io/nestweaver/commit/790631988cdee53b6c39b3c8dd50902937ce6769))
* **web:** implement SemanticZoom camera bridge and CommunityOverlay with 3D hulls ([48141bb](https://github.com/Kehl-io/nestweaver/commit/48141bbc5e3aeb700a5244cc5b9ef1ff63555b25))
* **web:** migrate auxiliary components and remove Sigma.js dependency ([a9afa1c](https://github.com/Kehl-io/nestweaver/commit/a9afa1c8fc0afba22fe862a243dca97edbacd0cb))
* **web:** migrate GraphPanel and mode hooks from Sigma.js to R3F ([18fcc85](https://github.com/Kehl-io/nestweaver/commit/18fcc85c6a4226085710ea4d7332d96a71ddd134))
* **web:** wire GlassPanel into panels and add impact ripple on selection ([b755127](https://github.com/Kehl-io/nestweaver/commit/b75512781b3a634a4c4dbf717da5a58fb83a3746))
* **web:** wire WASM engine end-to-end, fix navigation history, fix GlassPanel cursor light ([cec096e](https://github.com/Kehl-io/nestweaver/commit/cec096eebe4622f5cfa3b634aa5412f162fcc374))


### Bug Fixes

* --force re-index idempotency + broken-links surfaces unresolved wikilinks (QA) ([bec3d12](https://github.com/Kehl-io/nestweaver/commit/bec3d128e35c52507ec509de9897e95521b1abf6))
* add generation field to SSE events and update GPU picking docs ([fc26c87](https://github.com/Kehl-io/nestweaver/commit/fc26c87995670c934aa9d0c8ee09af4bfbd19f10))
* affected-tests reaches Jest/Vitest tests + consistent not-found exit codes (QA) ([dd0d48c](https://github.com/Kehl-io/nestweaver/commit/dd0d48cb4b545dbf42d996520be694b24c2ba122))
* regex line numbers + install-hook dry-run delta + multi-handler coverage (QA) ([c34bc1d](https://github.com/Kehl-io/nestweaver/commit/c34bc1d0737ecaa85ff8d36a0e2119110aaac465))
* resolve db from --config on brain read commands (Bug [#19](https://github.com/Kehl-io/nestweaver/issues/19)) ([e25c00e](https://github.com/Kehl-io/nestweaver/commit/e25c00e9c53d08c5561485dd49cb275bb4d47613))
* surface project member notes in project_context (Bug [#12](https://github.com/Kehl-io/nestweaver/issues/12)) ([38c9414](https://github.com/Kehl-io/nestweaver/commit/38c941408eed4ceaa011613abddffb86ed9a6876))
* **web:** fix edge rendering with depthTest disabled for visibility ([d4bc6d9](https://github.com/Kehl-io/nestweaver/commit/d4bc6d900ea6bcb47845a2d66cef4f33184adc37))
* **web:** replace LineSegments with instanced quads for visible edge rendering ([f036a6a](https://github.com/Kehl-io/nestweaver/commit/f036a6a61437b014500017d77de7e15d83eb91fb))
* **web:** run force layout to completion before rendering — no more flying nodes ([6cc0780](https://github.com/Kehl-io/nestweaver/commit/6cc07803039a00d6fe85a0aa5660a45817da67a5))
* **web:** strip Symbol/ prefix from kind in color lookup ([c488c28](https://github.com/Kehl-io/nestweaver/commit/c488c28281f1ffda15fa94c6e90a4874eb55503d))

## [0.10.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.9.0...nestweaver-v0.10.0) (2026-05-28)


### Features

* **cli:** show declared projects from --config alongside materialized ones in list-projects ([f336f93](https://github.com/Kehl-io/nestweaver/commit/f336f9314027fe5e35a89a67c12dd814c8e81e57))


### Bug Fixes

* add --config to read-only commands, EdgeType PPR classification, parallel parsing with rayon ([3c3392d](https://github.com/Kehl-io/nestweaver/commit/3c3392dd371bcba63f20ea1c0655cbd3915cd102))
* brain_status last_indexed, clusters exit code + adaptive resolution, get_summary diagnostics ([8e9540b](https://github.com/Kehl-io/nestweaver/commit/8e9540ba4ace42e23464071b9502d46b993356dc))
* **ci:** configure release-please to bump Cargo.toml workspace version automatically ([2d15106](https://github.com/Kehl-io/nestweaver/commit/2d15106fe3b9a52acaa7a59b3bb2ad803cd8d813))
* **engine:** add timeout and EOF detection to MCP client to prevent wiki_sources hang ([63efb37](https://github.com/Kehl-io/nestweaver/commit/63efb372db8fe6e8dfe2249362b42e28a56c4a5b))
* **engine:** robust MCP client with timeout, poisoning, reuse, and configurable timeout_secs ([18c04e8](https://github.com/Kehl-io/nestweaver/commit/18c04e8cdaf23b731039facc6a9f94975d73c778))
* **mcp:** tighten token budget with serialization-based final pass and relevance cost ([0f207b8](https://github.com/Kehl-io/nestweaver/commit/0f207b8e7c50e5880bab554d8b1e87ea82222fef))
* restore render_cost, add --config to all commands, bump version to 0.9.0 ([4ce7f3b](https://github.com/Kehl-io/nestweaver/commit/4ce7f3bd77f950b4252b8ab287b8c9873c54478e))
* **store:** deduplicate project nodes by name on upsert, correct MCP token budget measurement ([340a143](https://github.com/Kehl-io/nestweaver/commit/340a143bc77a6472244e464e416ca566bc1a4397))

## [0.9.0](https://github.com/Kehl-io/nestweaver/compare/nestweaver-v0.8.0...nestweaver-v0.9.0) (2026-05-28)


### Features

* agent interaction memory — PPR bias from usage patterns ([3e568f6](https://github.com/Kehl-io/nestweaver/commit/3e568f638c09f6f2133731820bcd2048e1179e9b))
* **cli:** add --name flag for repo display name override ([52e1c05](https://github.com/Kehl-io/nestweaver/commit/52e1c05a6a20a1d529e27301e8ff2eb44d08be48))
* **engine:** add InteractionTracker with event recording, consolidation, and decay ([175b54c](https://github.com/Kehl-io/nestweaver/commit/175b54c530826d9fb03c3f51de4012e3d7e153ef))
* **engine:** expand brain_search queries with taxonomy aliases for better recall ([2eaa63b](https://github.com/Kehl-io/nestweaver/commit/2eaa63b9abc3a99399485b9362b2d9823558f434))
* **engine:** unify brain_search to return both vault notes and code symbols ([db07420](https://github.com/Kehl-io/nestweaver/commit/db07420e310ad86484531dc41e3bcfd9d10e954b))
* **mcp:** add interaction telemetry hooks to MCP tool dispatch ([207b4cc](https://github.com/Kehl-io/nestweaver/commit/207b4cc66c8de2c26b3dffc4f731f2aeb2f62cce))


### Bug Fixes

* **cli:** collapse nested if to satisfy clippy::collapsible_if ([b59c3b4](https://github.com/Kehl-io/nestweaver/commit/b59c3b4252b4129c95b9588b4495b725fe0b416f))
* **cli:** match --repo filter against repo display name, not just file path ([5db4215](https://github.com/Kehl-io/nestweaver/commit/5db421503d2e73c97481af12d4984fa75f2d408d))
* **engine:** correct interaction memory implementation per research alignment ([a4b4985](https://github.com/Kehl-io/nestweaver/commit/a4b4985698089c071e2b68a36264d38becd2849a))
* **engine:** limit cluster summaries to top-50 largest, skip singletons ([0a7f777](https://github.com/Kehl-io/nestweaver/commit/0a7f777dc2e866353e561c9d0ab65c656375e5b9))
* repo_display_name in materialize, --db on list-links, positional watch, --repo filter on impact ([efbaeee](https://github.com/Kehl-io/nestweaver/commit/efbaeeec9b3dcb660b22a14a0fea273b210bbcca))

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
