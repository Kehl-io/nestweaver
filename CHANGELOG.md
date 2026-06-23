# Changelog

## [1.0.0](https://github.com/Kehl-io/nestweaver/compare/v1.1.3...v1.0.0) (2026-06-23)


### ⚠ BREAKING CHANGES

* embedding seed layer — local model semantic search with Metal acceleration ([#82](https://github.com/Kehl-io/nestweaver/issues/82))

### Features

* add remove-repo CLI command and daemon RPC ([0849715](https://github.com/Kehl-io/nestweaver/commit/08497152262ab70cc534f15ce905d26d4a360ccf))
* add watch config, simplify daemon lock error, apply lint fixes ([3fa34dd](https://github.com/Kehl-io/nestweaver/commit/3fa34dd53eb1dbd85a39b89853587bcae37876cb))
* agent guidance — hard rules in guides + subagent hook (F14, F15) ([6d8af5c](https://github.com/Kehl-io/nestweaver/commit/6d8af5c1fbbb6e41f2826d785b1b3161d14898a3))
* **algorithms:** add impact analysis BFS and substring search ([8befebe](https://github.com/Kehl-io/nestweaver/commit/8befebe4b38edfd82cd579b2597c8c609b2e0336))
* API contract graph — Contract nodes, IMPLEMENTS edges, drift (F2-core) ([ecdf8ad](https://github.com/Kehl-io/nestweaver/commit/ecdf8ad714b25b6b48a5f5604da47796c5e13b4f))
* AST-based type extraction via tree-sitter queries (zero re-parse cost) ([1b53093](https://github.com/Kehl-io/nestweaver/commit/1b5309367d4e3de06a43e22ba486a8d4434fe51b))
* **blast_radius:** confidence-weighted edge traversal for impact analysis ([40a1e6f](https://github.com/Kehl-io/nestweaver/commit/40a1e6f88486f293d5d5e2a53c26c85b4b7c3d66))
* brain.* document-graph tools (F9) ([b5664c2](https://github.com/Kehl-io/nestweaver/commit/b5664c2c4916f6e257426548942bfc8bc60bb503))
* **brain:** support cross-vault wikilinks with vault:note prefix ([b91c6e9](https://github.com/Kehl-io/nestweaver/commit/b91c6e91af57488e0085c82bf4c4c3212e7c2786))
* **brain:** wave-3 — diagnostics polish + brain context filters ([42f5af4](https://github.com/Kehl-io/nestweaver/commit/42f5af4844930a5bf5bd84c6d79cb336ecf5a4ed))
* **cli:** accept --track-interactions on daemon start with redirect message ([04783e0](https://github.com/Kehl-io/nestweaver/commit/04783e03b3dd38654b7a8defa254e9d278c01342))
* **cli:** add --daemon flag to route index through gRPC daemon ([29ab1ab](https://github.com/Kehl-io/nestweaver/commit/29ab1abbf71c59ef5db55b288e2b07e6b1e6c04a))
* **cli:** add --token-budget to nestweaver context ([587d292](https://github.com/Kehl-io/nestweaver/commit/587d2929ddceb3c8c52066293a4046445df97701))
* **cli:** add daemon routing to 10 high-frequency CLI commands ([3994ad8](https://github.com/Kehl-io/nestweaver/commit/3994ad853734b80063a5e0d7a18d424435e7d2b9))
* **cli:** add daemon start/stop/status subcommands ([2d8e567](https://github.com/Kehl-io/nestweaver/commit/2d8e56732ca8d4c0312d5e8d30fe8888a94872d1))
* **cli:** add remove-project and prune-stale commands ([6eed8d3](https://github.com/Kehl-io/nestweaver/commit/6eed8d3ed2328f2498f96bcf5325db0013d3869f))
* **cli:** add remove-repo command ([92f74d9](https://github.com/Kehl-io/nestweaver/commit/92f74d9cc744a7a247190359d98816a2a894341c))
* **client:** add materialize_projects, remove_vault, merge_instance, purge_instance methods ([012548a](https://github.com/Kehl-io/nestweaver/commit/012548ad50a33f8afe3f4875cd23403d9af1913e))
* **client:** add nestweaver-client crate with auto-start and version check ([763dd5e](https://github.com/Kehl-io/nestweaver/commit/763dd5eccc117c138bbd60e8f6d7e51a08805abc))
* **client:** add remove_project and prune_stale methods ([b6b4bb5](https://github.com/Kehl-io/nestweaver/commit/b6b4bb56b77f6ccf3b74a45f33f6defb0d2783ef))
* **client:** add remove_repo method ([9815993](https://github.com/Kehl-io/nestweaver/commit/9815993f63872962825d48bdee14450e4fc4f4b4))
* **cli:** implement snapshot build command ([9ed9158](https://github.com/Kehl-io/nestweaver/commit/9ed91581af29f6ec182dacedaac3bfebcedf5b8b))
* **cli:** implement snapshot push command ([07357dd](https://github.com/Kehl-io/nestweaver/commit/07357dd4d4092e452bab6b2e9d995df0f918670b))
* **cli:** instance_id from config, instance merge, and duplicate vault warning ([be0994d](https://github.com/Kehl-io/nestweaver/commit/be0994d054c52612c7d29a4afcd9dd3feac549e5))
* **cli:** route `brain search` through daemon Search RPC when available ([729d87d](https://github.com/Kehl-io/nestweaver/commit/729d87d5286caf29ce9eb68352b8cc207eecc919))
* **cli:** route brain watch through daemon when use_daemon=true ([83f2a53](https://github.com/Kehl-io/nestweaver/commit/83f2a537bd34bc481ed722e963d4969c49953eec))
* **cli:** wire [limits].default_result_limit to CLI search and context commands ([fc62e5b](https://github.com/Kehl-io/nestweaver/commit/fc62e5b032627086dc342c231c593c13cffb0e17))
* **config:** wire [limits].default_result_limit to runtime tool dispatch ([a9d4d12](https://github.com/Kehl-io/nestweaver/commit/a9d4d12e7165a925a1fcce8d5853c32c0c3ef045))
* daemon-based concurrent database access with performance optimizations ([c32c54e](https://github.com/Kehl-io/nestweaver/commit/c32c54eae8779db133b3280abe1688080bebeed5))
* **daemon:** add nestweaver-daemon crate with gRPC server scaffold ([cd0b248](https://github.com/Kehl-io/nestweaver/commit/cd0b2486081ef0cc85a2573856ae858bb9d659d5))
* **daemon:** daily log rotation via tracing-appender with non-blocking writer ([9f86eae](https://github.com/Kehl-io/nestweaver/commit/9f86eae562cbf16ab5acacd99893329ad96212f7))
* **daemon:** eliminate all stop_daemon_if_running calls ([ab63f4b](https://github.com/Kehl-io/nestweaver/commit/ab63f4b9692fe786b6bcd740283a87248400e4f7))
* **daemon:** extend try_daemon_json_rpc with 19 additional RPC method routes ([2bae19e](https://github.com/Kehl-io/nestweaver/commit/2bae19e28b520062e13cf30e9785795fd290501b))
* **daemon:** implement IndexRepo and IndexVault streaming RPCs ([0876578](https://github.com/Kehl-io/nestweaver/commit/087657892a698dd1f37469af143698b098f5dadf))
* **daemon:** implement MaterializeProjects, RemoveVault, MergeInstance, PurgeInstance handlers ([15057e5](https://github.com/Kehl-io/nestweaver/commit/15057e589eb7a049b7652422a799fafbce02dab8))
* **daemon:** implement RemoveProject and PruneStale handlers ([447bf4b](https://github.com/Kehl-io/nestweaver/commit/447bf4b0f3c0e696ff21d8707ecf361481602da0))
* **daemon:** implement RemoveRepo RPC handler ([d6b6d95](https://github.com/Kehl-io/nestweaver/commit/d6b6d95e4bb717d60cf49c50cbb4ffd7e0c2f724))
* **daemon:** implement WatchVault and StopWatch RPCs ([bcfcc76](https://github.com/Kehl-io/nestweaver/commit/bcfcc76df03124b0a4d41f8a1523baf384855ec6))
* **daemon:** load InstanceConfig for ranking-prior parity in tool dispatch ([19b8789](https://github.com/Kehl-io/nestweaver/commit/19b878901c5b7da9ff319ee4d0608bd1330749cc))
* **daemon:** set process title to nestweaver-daemon-{id} for pgrep ([dddbb43](https://github.com/Kehl-io/nestweaver/commit/dddbb4325b086fa9744b25559d848cf4bba3cb8c))
* default MCP and CLI to daemon mode, add --no-daemon escape hatch ([642c332](https://github.com/Kehl-io/nestweaver/commit/642c3321399d3f7ebe544e15f3d08c6f2c933a62))
* edge evidence arrays + full language parity (27 tree-sitter, 19 type queries) ([a9b68b5](https://github.com/Kehl-io/nestweaver/commit/a9b68b5f91e4985d3e5450bd03cb3e34166721d0))
* embedding seed layer — local model semantic search with Metal acceleration ([#82](https://github.com/Kehl-io/nestweaver/issues/82)) ([aa4f9d4](https://github.com/Kehl-io/nestweaver/commit/aa4f9d424b6fe2dd9cb020f55eaf2a110a4a0c6a))
* **engine:** add read_symbols — symbol-window source reads (F5) ([ec5cfb0](https://github.com/Kehl-io/nestweaver/commit/ec5cfb060d5f1b0605d8e68c7703927d2dc5d687))
* **engine:** affected_tests — static RTS for PR test selection (F13) ([21607bc](https://github.com/Kehl-io/nestweaver/commit/21607bce01acf0cd7c4933dff9e46b4cebaa58fd))
* **engine:** co-change mining from git history with Jaccard scoring ([21ce795](https://github.com/Kehl-io/nestweaver/commit/21ce7959346112291a8529b9857f6c286f0c0592))
* **engine:** finish agent feedback loop — TerminalSuccess + interactions show (F1) ([3bcfeaa](https://github.com/Kehl-io/nestweaver/commit/3bcfeaa118ec435a0f1bd3f31c489d78ff9aa095))
* **engine:** implement memory consolidate --apply ([b06f721](https://github.com/Kehl-io/nestweaver/commit/b06f721f01372d6f41f486f26b9decded512c53d))
* **engine:** inline high-confidence result bodies (F8) ([76712ac](https://github.com/Kehl-io/nestweaver/commit/76712ace4177e6077cb1ce511a5b70d648d9bd75))
* **engine:** investigate bundle primitive (F10) ([ee1361c](https://github.com/Kehl-io/nestweaver/commit/ee1361cb62edeed2d3504ba176586fc55d144044))
* **engine:** lightweight result reranker (F17) ([baf1e5b](https://github.com/Kehl-io/nestweaver/commit/baf1e5bf42165683685c1f5b034f85783a69914b))
* **engine:** per-path dampen/boost ranking priors (F6) ([545767a](https://github.com/Kehl-io/nestweaver/commit/545767ae5ade35593fa494280194ed8402d18a92))
* **engine:** retrieval-quality eval harness (P0.3) ([af5421b](https://github.com/Kehl-io/nestweaver/commit/af5421bdaf5a5445ff04fbecfb81b38cd8d16945))
* **engine:** rewrite wikilinks after consolidate --apply moves files ([8924a01](https://github.com/Kehl-io/nestweaver/commit/8924a01901e7ead210b28a21c37545b7ba41cb27))
* enrich brain_guide/admin instructions with tool-routing table, add staleness warnings and token hints ([45d90ad](https://github.com/Kehl-io/nestweaver/commit/45d90ad4fedd33d672f3ac935cc89778a6d979cd))
* **export:** read and surface edge evidence in cypher export ([aa957d9](https://github.com/Kehl-io/nestweaver/commit/aa957d91bb60bf516ad80316414c4a8e4fc7c363))
* git-activity-dampened CodeRank (F12) ([fd4acbc](https://github.com/Kehl-io/nestweaver/commit/fd4acbcca1a103351181b9cb7d0f795d6f62e52f))
* **graph:** add node preview card component ([3c0664c](https://github.com/Kehl-io/nestweaver/commit/3c0664cd596427fdd908e4d0cb89379b45d286fd))
* **graph:** add preview card state to store ([95b600e](https://github.com/Kehl-io/nestweaver/commit/95b600e7554a50e4303a5c65a67504dca8900b4e))
* **graph:** add useNodePreview hook with LRU cache ([a5583ab](https://github.com/Kehl-io/nestweaver/commit/a5583ab0e7fd03fc2e9649870918be63cbc9a648))
* **graph:** Obsidian-style visual reskin + click-to-preview UX ([ca33849](https://github.com/Kehl-io/nestweaver/commit/ca338491d7115d8c634c3d5df3a6d9ad15d0cba1))
* **graph:** populate context menu with grouped power actions ([57bed27](https://github.com/Kehl-io/nestweaver/commit/57bed27117e21478d859ead6449518080a0f1acb))
* **graph:** wire click-to-preview and escape dismiss ([4a8a4b0](https://github.com/Kehl-io/nestweaver/commit/4a8a4b08f8fcf23864668fd0f85ac085f536fe63))
* **guide:** add CLAUDE.md generation format ([944fa94](https://github.com/Kehl-io/nestweaver/commit/944fa948abc0636ee2104989054b6def3c54a94d))
* implement stubbed features, daemon-subsumes-watcher, production hardening ([ec6af24](https://github.com/Kehl-io/nestweaver/commit/ec6af241651b03d35c28779a324804cf49688e5b))
* implement stubbed snapshot and consolidate features ([6019903](https://github.com/Kehl-io/nestweaver/commit/6019903ad0e6762261af2538a1e10f2714a9d397))
* improve graph UI readiness ([5c17279](https://github.com/Kehl-io/nestweaver/commit/5c172794bd96630c2a1be0d6a9632677ec5bbe13))
* **index:** build TypeEnvironments per file during indexing (not yet used for resolution) ([c326d47](https://github.com/Kehl-io/nestweaver/commit/c326d4750a9e674ef2f1ea13d7fc99ff60277586))
* **index:** emit MEMBER_OF edges from parent_name during indexing ([1008216](https://github.com/Kehl-io/nestweaver/commit/10082169e5781082b2036f8a9c730c1259236e23))
* **instance-remove:** add --purge-graph to cascade-delete instance data ([5377f83](https://github.com/Kehl-io/nestweaver/commit/5377f83cc4a12ae1f8ad3f5e7feafe687ad6d339))
* **interactions:** lower flush threshold to 5 and add time-based auto-flush ([8e7d83c](https://github.com/Kehl-io/nestweaver/commit/8e7d83c1d39b35196f59e51a76253c560c923d1c))
* **investigate:** surface body truncation via `body_complete` + newline-aware cuts ([91f0e37](https://github.com/Kehl-io/nestweaver/commit/91f0e3736ce11be7d7689a3313e8b5b888e30673))
* markdown knowledge graph enhancements — CLAUDE.md gen, AgentConfig, canvas/dataview/mermaid parsers ([bc40f77](https://github.com/Kehl-io/nestweaver/commit/bc40f77b72c7da53f13e8d1a8b4947135720bb49))
* **mcp:** add brain_remove_source and prune_stale tools ([8853ce5](https://github.com/Kehl-io/nestweaver/commit/8853ce5ffb91290f3de77d5499480e04074d0646))
* **mcp:** add daemon proxy mode with --daemon flag ([9536c4e](https://github.com/Kehl-io/nestweaver/commit/9536c4ed86866cb506564538539f39051eecde18))
* **mcp:** expand interaction tracking to cover more tools ([ce4e345](https://github.com/Kehl-io/nestweaver/commit/ce4e345a53f0a300f7c49316fda222052b1871ba))
* memory-bank semantics — typed edges, lint, consolidate, related (F11) ([3421e5d](https://github.com/Kehl-io/nestweaver/commit/3421e5d636014561b1590c21c7a0de27bd8c64e4))
* next-gen R3F web UI, algorithms/WASM crates, and v0.9.1 retrieval quality + eval harness ([9792d7f](https://github.com/Kehl-io/nestweaver/commit/9792d7fc8f98c0a48e96c2b447885daf72a558c2))
* **parser:** add C/Elixir type queries, fix Swift receivers, add Lua self binding ([0cf4183](https://github.com/Kehl-io/nestweaver/commit/0cf4183f17f1d5e0c8b0383248f334c09313e159))
* **parser:** add constructor patterns to Kotlin, Dart, Swift, Scala, C#, SystemVerilog type queries ([83ca933](https://github.com/Kehl-io/nestweaver/commit/83ca9333a37ec451f679439fceb2eaf0f90a2226))
* **parser:** add Dataview DQL query parser ([427697b](https://github.com/Kehl-io/nestweaver/commit/427697b1ee4ec0f082bc3a81e5914756b9c128a5))
* **parser:** add Go interface methods, array types + Python class attributes, instance properties ([02af24f](https://github.com/Kehl-io/nestweaver/commit/02af24f1ebf56ab85902e2b7e35880d3ae9b56f1))
* **parser:** add interface method declarations and instance property captures across languages ([35c22ed](https://github.com/Kehl-io/nestweaver/commit/35c22ede09b55932787744f001381888526df177))
* **parser:** add Mermaid flowchart/graph diagram parser ([0fab1e2](https://github.com/Kehl-io/nestweaver/commit/0fab1e241cd7f8f5dbdcf06dfdd9ac93d8166af3))
* **parser:** add Obsidian canvas file parser ([ecd27f5](https://github.com/Kehl-io/nestweaver/commit/ecd27f53ca14939dcd52607753674b096f5df480))
* **parser:** add tree-sitter type query files for 5 languages ([ad3e816](https://github.com/Kehl-io/nestweaver/commit/ad3e816ae65257c0f66c2bf6db8ff86daa03aae9))
* **parser:** add type extraction queries for C++, C#, Kotlin, PHP, Dart, Swift, Scala, Ruby ([980467a](https://github.com/Kehl-io/nestweaver/commit/980467a6f0908ca4b4d1add37106e7b15d8bc444))
* **parser:** add type queries and self bindings for remaining OOP languages ([2006071](https://github.com/Kehl-io/nestweaver/commit/200607154660451437e331b5baeadbd9f2233c43))
* **parser:** add visibility inference for Ruby, PowerShell, Fortran, Pascal, SystemVerilog, Julia ([deebd8a](https://github.com/Kehl-io/nestweaver/commit/deebd8a391a34218fca12056297694f96459c480))
* **parser:** expand receiver extraction to all languages for type_aware resolution ([cea866a](https://github.com/Kehl-io/nestweaver/commit/cea866aeef7d13896ca30dbfde631ef6dcd86dcc))
* **parser:** expand Rust type queries with struct constructors and destructuring ([0e31539](https://github.com/Kehl-io/nestweaver/commit/0e31539a6b3af344613cf1cddafc9b23c52d0007))
* **parser:** expand type queries for TS, Python, Go, Java ([15f93a5](https://github.com/Kehl-io/nestweaver/commit/15f93a5bf5c34ba1fc0ca500f71edba6cc39eb61))
* **parser:** extend parent_name to fields/properties for MEMBER_OF expansion ([ef71a3d](https://github.com/Kehl-io/nestweaver/commit/ef71a3df325174b40a71d59daaa2345ba59b6596))
* **parser:** extract checkboxes and ADR sections from markdown ([2289d8e](https://github.com/Kehl-io/nestweaver/commit/2289d8e842e5897b8975998aa5bf4d1a43a7afeb))
* **parser:** extract parent class/struct name for method symbols ([303fcce](https://github.com/Kehl-io/nestweaver/commit/303fcce0e0b91c53d19ff86626656b620932e9c5))
* **parser:** extract receiver from method call expressions ([09183e8](https://github.com/Kehl-io/nestweaver/commit/09183e8e2a81e4233aaf8aa74a6e89c8a6ceb2fa))
* **parser:** upgrade Fortran, Pascal, SystemVerilog from regex to tree-sitter ([700d085](https://github.com/Kehl-io/nestweaver/commit/700d085bc9ccba040cf0afacab0f8a95dd565d99))
* **parser:** upgrade Groovy, Zig, Objective-C from regex to tree-sitter ([5061a67](https://github.com/Kehl-io/nestweaver/commit/5061a67ffc5522b93b41de4dd5be4555be1d19f5))
* **parser:** upgrade PowerShell, Julia, SQL, HCL from regex to tree-sitter ([ca4d3b3](https://github.com/Kehl-io/nestweaver/commit/ca4d3b31d49730237ec8fcb5b0e49f348b572949))
* **parser:** walk tree-sitter AST for type bindings (zero re-parse cost) ([8da0084](https://github.com/Kehl-io/nestweaver/commit/8da0084103c711da0d92c5ec0d1348b732b304a7))
* persisted graph_generation (P0.2) + ZSTD response cache (F16) ([f54538a](https://github.com/Kehl-io/nestweaver/commit/f54538a652566b93fa63d685d4960da98fe6d15b))
* **project:** wire tags, parent, and features from ProjectConfig ([3f1df68](https://github.com/Kehl-io/nestweaver/commit/3f1df688f1cac22c0f6a4e4980e820e642cb6309))
* **proto:** add MaterializeProjects, RemoveVault, MergeInstance, PurgeInstance RPCs ([69a8b99](https://github.com/Kehl-io/nestweaver/commit/69a8b99d54787558e41c305d83c5c965321aae16))
* **proto:** add nestweaver-proto crate with daemon gRPC service definition ([03394ea](https://github.com/Kehl-io/nestweaver/commit/03394ea7e17ed3e161c536010c85bfb095868633))
* **proto:** add RemoveProject and PruneStale RPCs ([a7b165e](https://github.com/Kehl-io/nestweaver/commit/a7b165ea2f6329282be6e673cdade481fd7304e8))
* **proto:** add RemoveRepo RPC message and service method ([7123881](https://github.com/Kehl-io/nestweaver/commit/712388162fd61d044d497ea132a677e7c6e69ce9))
* **proto:** add WatchVault and StopWatch RPCs ([8d3d3a6](https://github.com/Kehl-io/nestweaver/commit/8d3d3a64bc515128ccf5da8035a7b54fdf587935))
* **proto:** extend BrainContextRequest for daemon parity ([eecee83](https://github.com/Kehl-io/nestweaver/commit/eecee83dfed64d1357f4a240782c9d59710e9bd1))
* **proto:** forward since/recency_weight through ProjectContextRequest ([770e3c2](https://github.com/Kehl-io/nestweaver/commit/770e3c2971122bc942b6f31f156200e845073953))
* **proto:** typed protobuf messages for 6 hot-path RPCs ([ebccec6](https://github.com/Kehl-io/nestweaver/commit/ebccec67687793fadbb9074d5147aae7cb26186a))
* **resolver:** add import resolvers for Scala, Groovy, Fortran, Pascal, SystemVerilog, Zig, ObjC, Lua, PowerShell, Julia ([92cdedf](https://github.com/Kehl-io/nestweaver/commit/92cdedf29d0f185f131f9701aa7ff1fe4edef7fc))
* **resolver:** add per-language type extractors for annotations and constructors ([89fe757](https://github.com/Kehl-io/nestweaver/commit/89fe75735d5da4f658a7e83c9290c606541d8f77))
* **resolver:** cross-file return type propagation for type-aware resolution ([af4cae8](https://github.com/Kehl-io/nestweaver/commit/af4cae84626209c0e298f9a45e5b4f35bf84f0e8))
* **resolver:** decompose chained dot receivers for type-aware resolution ([95a8939](https://github.com/Kehl-io/nestweaver/commit/95a8939f591f0a1bf6678c09aec90e998cc3b289))
* **resolver:** feed AST type bindings into TypeEnvironment as primary source ([3ce752b](https://github.com/Kehl-io/nestweaver/commit/3ce752b0e95803a089562b9c638e1f4690270c59))
* **resolver:** improve assignment extraction with dotted paths and function calls ([f3de51b](https://github.com/Kehl-io/nestweaver/commit/f3de51b988fe2f47ef4aa57fec6db662b485937a))
* **resolver:** MRO walk for inherited methods in type-aware resolution ([97ae87e](https://github.com/Kehl-io/nestweaver/commit/97ae87e39adcbe5f17ac0f7ba4f37c189b9fd053))
* **resolver:** populate evidence array at each resolution step ([2627737](https://github.com/Kehl-io/nestweaver/commit/26277379e9e0fd90311e53de8d49943b431181a9))
* **resolver:** type-aware member call resolution using TypeEnvironment ([989ecc8](https://github.com/Kehl-io/nestweaver/commit/989ecc888e337e5cdb861d9d896c2be1b40860f4))
* **resolver:** TypeEnvironment with 4-tier inference and fixpoint propagation ([b27eb0a](https://github.com/Kehl-io/nestweaver/commit/b27eb0aec1c353c5a007aff4a787b35f1803cb8d))
* route all database writes through daemon RPC ([39b0eba](https://github.com/Kehl-io/nestweaver/commit/39b0eba208db039798fc402b7624f2c359d5aba7))
* **schema:** add ALL_SYMBOL_EDGE_TYPES constant and from_rel_table_name lookup ([e7f0d35](https://github.com/Kehl-io/nestweaver/commit/e7f0d353558ecb8a4b829c46b0a98556b7502c49))
* **schema:** add EdgeEvidence struct and evidence field to ResolvedEdge ([3dfd3fb](https://github.com/Kehl-io/nestweaver/commit/3dfd3fb4e9e03056198eea53e8c686c13a5019ac))
* **schema:** add NoteKind::AgentConfig for agentic instruction files ([e0f3041](https://github.com/Kehl-io/nestweaver/commit/e0f30418d92fe4c610a69f07b2410d306ddf99a8))
* **schema:** add Symbol.end_line spanning a symbol's full source range (P0.1) ([7666842](https://github.com/Kehl-io/nestweaver/commit/7666842fbdd79f65a1d6e7a20998df520764e82b))
* **seed-resolution:** graduated path-deboost + kind-priority for symbol lookup ([ab42eed](https://github.com/Kehl-io/nestweaver/commit/ab42eed43e67b2bca26acf38f8f69dc74e6e1fde))
* **setup:** add PreToolUse hook to intercept grep/find, fix token savings claims to validated 10x ([fc139bf](https://github.com/Kehl-io/nestweaver/commit/fc139bfdd7b5d083397dc87e2eb29d6ae99a49af))
* **setup:** install Claude Code hooks, add token-savings messaging to skills and cursor rules ([ed6e7b0](https://github.com/Kehl-io/nestweaver/commit/ed6e7b0198c996c838a56421bfad935de87b3478))
* **snapshot:** switch to per-file checksums covering sidecars ([a2c99cd](https://github.com/Kehl-io/nestweaver/commit/a2c99cdd3638bcdeadecf4ad7b91a08d165e250f))
* **store:** BM25 pseudo-relevance feedback (PRF) (F7) ([2abd40b](https://github.com/Kehl-io/nestweaver/commit/2abd40b48148151ab0d2a47b41d73b489463dbc3))
* **store:** config-driven test path deboost patterns ([cdeae48](https://github.com/Kehl-io/nestweaver/commit/cdeae48efe92ad109d914ed907fdb89dfb1df836))
* **store:** persist edge evidence as JSON property ([80690f5](https://github.com/Kehl-io/nestweaver/commit/80690f55f71616e66535daab125eb7924385c128))
* **store:** surface FILE_HAS_SYMBOL as DEFINES edges in typed edge export ([c310987](https://github.com/Kehl-io/nestweaver/commit/c3109875be981d86a787f72e8f3d5bc4df52f417))
* **store:** trigram-accelerated regex_search + count_patterns (F3, F4) ([2212a3c](https://github.com/Kehl-io/nestweaver/commit/2212a3cfd8159e3ed57cd36aa56ec92200ef97aa))
* type-aware call resolution, confidence-weighted impact analysis, co-change mining ([88072a5](https://github.com/Kehl-io/nestweaver/commit/88072a59230f8628daa0087a0d07274912c3d453))
* update dependencies, launchd daemon, and macOS app bundle ([#88](https://github.com/Kehl-io/nestweaver/issues/88)) ([fbeda38](https://github.com/Kehl-io/nestweaver/commit/fbeda38821038b3bc6a5297513dd428b93366a9e))
* **wasm:** build WASM binary and wire worker/bridge to real wasm-bindgen API ([c66b73f](https://github.com/Kehl-io/nestweaver/commit/c66b73f02ca96c6ed5875d01a2435c090b743838))
* **web:** add contextual graph actions ([eaa16e3](https://github.com/Kehl-io/nestweaver/commit/eaa16e3d88e4a3ea7b513fb5e18aa7fdc345e397))
* **web:** add dense graph views ([ffb04a6](https://github.com/Kehl-io/nestweaver/commit/ffb04a6b92c4b377b48d5b55bba2ae7d50fef066))
* **web:** add edge gradients, keyboard graph navigation, and node drag ([b3cf7ca](https://github.com/Kehl-io/nestweaver/commit/b3cf7ca05ea83d7f0e81421f61c86f089137fd35))
* **web:** add node labels for seed, selected, and hovered nodes ([0d04cdb](https://github.com/Kehl-io/nestweaver/commit/0d04cdb06f82f67f9aa827499a30ac2ad16b97d3))
* **web:** add Obsidian-style graph polish — animated settling, always-visible labels, click-to-focus ([8551857](https://github.com/Kehl-io/nestweaver/commit/8551857c2769e30451b8272c4d051f6687ca5299))
* **web:** add overview API ([a1041dd](https://github.com/Kehl-io/nestweaver/commit/a1041dd1dfcd5701f52a5d6c284e53ec6f11740d))
* **web:** add overview frontend types ([f3e7a69](https://github.com/Kehl-io/nestweaver/commit/f3e7a69602cb1b0b232b1a13ce279c4aba739111))
* **web:** add overview guidance surfaces ([d037bd4](https://github.com/Kehl-io/nestweaver/commit/d037bd4c01998afac9819ee0d3c3d3bfffc37111))
* **web:** bring graph nodes to life ([6a19f35](https://github.com/Kehl-io/nestweaver/commit/6a19f3536d35a220916f1be1c6f2d53d0a34c8f6))
* **web:** group graph controls ([83a5fe1](https://github.com/Kehl-io/nestweaver/commit/83a5fe1321e8c824f8b06083baf5bc9824719ba8))
* **web:** implement SemanticZoom camera bridge and CommunityOverlay with 3D hulls ([78749d0](https://github.com/Kehl-io/nestweaver/commit/78749d06daa5c18127fb71c91c41784df133f9a2))
* **web:** load overview graph by default ([42ce763](https://github.com/Kehl-io/nestweaver/commit/42ce763db92324d29eccd3b012bc0498bf2941fb))
* **web:** wire GlassPanel into panels and add impact ripple on selection ([96bb7a8](https://github.com/Kehl-io/nestweaver/commit/96bb7a80d5fa5bebd925d20b683ec7a1dbcad7b2))
* **web:** wire WASM engine end-to-end, fix navigation history, fix GlassPanel cursor light ([dd60260](https://github.com/Kehl-io/nestweaver/commit/dd6026032f90b656040836b31e56b7c827e4afc7))
* wire [limits].default_result_limit to runtime dispatch ([628509c](https://github.com/Kehl-io/nestweaver/commit/628509c43bf79b185fda7a205a7ece494887a639))


### Bug Fixes

* --force re-index idempotency + broken-links surfaces unresolved wikilinks (QA) ([9aae607](https://github.com/Kehl-io/nestweaver/commit/9aae6073e291898dfb20db04cb022c523fb66178))
* add evidence to store/ranking tests, remove unused tree-sitter-sql dep ([52518ea](https://github.com/Kehl-io/nestweaver/commit/52518eada77b7250eb225ec5186cf3f74543f50d))
* add generation field to SSE events and update GPU picking docs ([81aeeed](https://github.com/Kehl-io/nestweaver/commit/81aeeedb5d79199f6d8c275e9a2dafa8b582f314))
* address all minor review findings — deterministic resolvers, Ruby visibility docs, ObjC fallback docs, fix dotted assignment test ([7879a5d](https://github.com/Kehl-io/nestweaver/commit/7879a5dd3755870e45d6be8d5434470e4d96adc4))
* address code review findings ([c9242ea](https://github.com/Kehl-io/nestweaver/commit/c9242eaead31bdb5d7a66bcad43e01b73d1e86ce))
* address code review findings — AgentConfig round-trip, drive letter guard, leaked configs, dead code, dataview tag regex ([4065ff1](https://github.com/Kehl-io/nestweaver/commit/4065ff10b7e68939618e9f719bb72a84cb5ddf08))
* address code review findings — remove misleading Julia visibility, fix self-referential assignments, remove redundant SV check ([407f0e9](https://github.com/Kehl-io/nestweaver/commit/407f0e9b3b1e6db1e570b8166ff4b8e2ad7acf74))
* address code review findings for production readiness ([731b449](https://github.com/Kehl-io/nestweaver/commit/731b449a133a6723596c04c9f69c96b03baa8c26))
* address final review minor items ([3bfc393](https://github.com/Kehl-io/nestweaver/commit/3bfc3934fa494f2bd701f13fed9177551e754f9a))
* address review findings for production readiness ([b9692a8](https://github.com/Kehl-io/nestweaver/commit/b9692a80df463454fa78bbef0b19e2680efda074))
* affected-tests reaches Jest/Vitest tests + consistent not-found exit codes (QA) ([0e03458](https://github.com/Kehl-io/nestweaver/commit/0e03458189bee51c0c626397e3453ed47e1d66e2))
* apply rustfmt and clippy fixes ([f99bc78](https://github.com/Kehl-io/nestweaver/commit/f99bc78d8dcf15ab44bbb2b480a710fccbe14387))
* **app:** use template image for menubar icon with transparent background ([7e30ce7](https://github.com/Kehl-io/nestweaver/commit/7e30ce7540a5088045d9c70525043494214bc990))
* **brain-remove:** match stored vault paths regardless of canonical form ([17c0b83](https://github.com/Kehl-io/nestweaver/commit/17c0b83583560d629cae500b8b6d417d278ae039))
* **brain-status:** expose instance_id per row + forward collision warnings through daemon ([44fe8b0](https://github.com/Kehl-io/nestweaver/commit/44fe8b09ecc15f78ffca879e292700b51b601d92))
* **brain:** close four v0.21.0 regressions surfaced by live-DB audit ([e76f769](https://github.com/Kehl-io/nestweaver/commit/e76f769a09dd8d9ccf856e4587a7c63aeeab29b6))
* **brain:** remove ghost vault rows by path match, not just computed UID ([9488e4c](https://github.com/Kehl-io/nestweaver/commit/9488e4cf216ca25b06dbadb6f3f75c6521bc0274))
* bump gRPC message size limit from 64MB to 256MB (fixes export cypher) ([cfcacb5](https://github.com/Kehl-io/nestweaver/commit/cfcacb54d1f8db2ef7127c6e8f07d893cb2c82da))
* **ci:** add --workspace to clippy job and fix pre-existing test errors ([cd8f40a](https://github.com/Kehl-io/nestweaver/commit/cd8f40a2c2e8b6c86b7de052166c30a0d4399a25))
* **ci:** add protobuf-compiler to CI, fix rustfmt formatting ([7d65f7c](https://github.com/Kehl-io/nestweaver/commit/7d65f7c4b097735fbbf721b0b6e15db1405f2f57))
* **ci:** add protobuf-compiler to release build workflow ([1428f1b](https://github.com/Kehl-io/nestweaver/commit/1428f1b1a0cfbd33c6371ef9e40fa1de1145d51b))
* **ci:** eliminate zstd link errors and skip daemon tests on Linux ([b002bfa](https://github.com/Kehl-io/nestweaver/commit/b002bfae28417bb90eb1158f5e41003171f79bc8))
* **ci:** resolve clippy lints and add NESTWEAVER_NO_DAEMON to CI test/coverage jobs ([5e381ea](https://github.com/Kehl-io/nestweaver/commit/5e381ea91da4d7bdaf719698443db5518679bd94))
* **ci:** set NESTWEAVER_NO_DAEMON=1 for E2E tests ([4539751](https://github.com/Kehl-io/nestweaver/commit/4539751f47bafb3211d05474c90cd600a0212cb7))
* **cli:** add --limit to broken-links, orphans, topic-clusters, tag-graph; surface staleness_commits_behind ([29f6862](https://github.com/Kehl-io/nestweaver/commit/29f68623ccc5529fc9ec37fdc890bda1badea92a))
* **cli:** add --limit to broken-links, orphans, topic-clusters, tag-graph; surface staleness_commits_behind ([80c478e](https://github.com/Kehl-io/nestweaver/commit/80c478ec42ed9c454299bddb41a990eba6c40544))
* **cli:** add daemon RPCs for list-repos/services/projects/search/symbol ([e6ac190](https://github.com/Kehl-io/nestweaver/commit/e6ac190b71679ed59683a8ea519edd0ee2a73e9f))
* **cli:** add stale-check subcommand, body_complete indicator, and snapshot output default ([cf3bad8](https://github.com/Kehl-io/nestweaver/commit/cf3bad8cfb026689ef06d9e14ba9cdb02807fbb1))
* **cli:** allow --db after daemon subcommand with global arg ([d9f62b9](https://github.com/Kehl-io/nestweaver/commit/d9f62b9e57ad05159c5bf27ac258b261e5db861d))
* **cli:** atomically detect running daemon via pidfile flock in daemon start ([cba759c](https://github.com/Kehl-io/nestweaver/commit/cba759caf937ad01b24ed095200420bda97ddfbc))
* **client:** correct daemon spawn arg order and increase socket timeout to 5s ([7256f5c](https://github.com/Kehl-io/nestweaver/commit/7256f5ca5114a3d07945b5372d78b6f7a7ab8709))
* **client:** fix tonic UDS client connection for 0.13 ([5b427ee](https://github.com/Kehl-io/nestweaver/commit/5b427ee0aa812df09ad25507625f55ec0311faf0))
* **cli:** graceful daemon fallback for ui/watch, fix collapsible-if clippy warnings ([1210109](https://github.com/Kehl-io/nestweaver/commit/1210109217ef2444b483bc59d7b7d94a2b9a7334))
* **cli:** guard --no-daemon for testing, route brain-remove/snapshot through daemon ([ad626dd](https://github.com/Kehl-io/nestweaver/commit/ad626dd9cec01822211af4c3fc4b725924981b88))
* **cli:** guard --no-daemon in MCP server command consistently ([5878908](https://github.com/Kehl-io/nestweaver/commit/5878908f5872bcd92dde9c5d5d0ed3b131ebd094))
* **cli:** keep direct-write fallback for index/brain-add when NESTWEAVER_NO_DAEMON=1 ([f7054e4](https://github.com/Kehl-io/nestweaver/commit/f7054e471733f64ab3c6189f78d10fd4776cc3a0))
* clippy/fmt fixes + docs for remove-project, prune-stale, brain_remove_source ([f0bc34d](https://github.com/Kehl-io/nestweaver/commit/f0bc34d5f1129ad9392a308aebbc55de4d709880))
* **cli:** rename UnlinkedVault to DiscardedVault, clarify merge output ([27b797e](https://github.com/Kehl-io/nestweaver/commit/27b797e1247da1d770d9dace384e1d31b1edf3c5))
* **cli:** resolve clippy warnings in daemon routing guards ([1df9c95](https://github.com/Kehl-io/nestweaver/commit/1df9c958976457c357c2c1c77582ac64071bf10f))
* **cli:** route 10 read commands through daemon, improve lock error message ([283c6e2](https://github.com/Kehl-io/nestweaver/commit/283c6e2fc38331b963702d5e94f6931fdd63dfee)), closes [#63](https://github.com/Kehl-io/nestweaver/issues/63)
* **cli:** route all 5 json-gated commands through daemon unconditionally ([6bbe00a](https://github.com/Kehl-io/nestweaver/commit/6bbe00a3b51bd5375c0a3db52c75fd1847314b95))
* **cli:** route brain context and 5 docgraph commands through daemon ([abb75b4](https://github.com/Kehl-io/nestweaver/commit/abb75b45328594de9f66be976e1a8615c06695fb))
* **cli:** route context/cross-repo-refs/generate-guide/cluster through daemon ([11a1fd6](https://github.com/Kehl-io/nestweaver/commit/11a1fd6415520208d60d2c47be6e930412928396))
* **cli:** route export and ui commands through daemon stop/restart ([cd3f95a](https://github.com/Kehl-io/nestweaver/commit/cd3f95a1e66a6e9d8961a38c5b6ae4d6cdc6a386))
* **cli:** route impact, regex-search, hubs, stale-check, status through daemon ([aba0fa0](https://github.com/Kehl-io/nestweaver/commit/aba0fa05524e7c7e6e6cacfdaa6f0ca1035503bf))
* **cli:** route remove-repo/remove-project resolution through daemon RPCs ([9d9ad3f](https://github.com/Kehl-io/nestweaver/commit/9d9ad3f5eecb4c835cb0142d4b5a089da099f054))
* **cli:** route repo-map/suggest-links/detect-implicit-projects/pr-impact through daemon ([ed7aeeb](https://github.com/Kehl-io/nestweaver/commit/ed7aeeb39517b0a2ba04a8ac717bb47c35301670))
* **cli:** route watch/export/ui through daemon (zero bypasses remaining) ([b23b409](https://github.com/Kehl-io/nestweaver/commit/b23b409a30d8efa3f03c31baff85d81c090b712c))
* **cli:** silence stop_watch errors when daemon is already gone ([e89736f](https://github.com/Kehl-io/nestweaver/commit/e89736f17a34dbe0144fd53a2bd1db943d4b9930))
* **cli:** stop daemon before direct-mode index to prevent lock contention ([e3f4f90](https://github.com/Kehl-io/nestweaver/commit/e3f4f907e0cc23825187425ad23fd42fbb119f96))
* **cli:** thread --config through autostart so auto-spawned daemons get ranking priors ([e5ed23d](https://github.com/Kehl-io/nestweaver/commit/e5ed23d8ab992e11d03ed9d23b88c44bd11a795c))
* **cli:** verify snapshot integrity after instance pull ([a2eb7cc](https://github.com/Kehl-io/nestweaver/commit/a2eb7cca2e41cf979d775b269571ae5ff8466fc6))
* **cli:** write Claude Code MCP config to .mcp.json instead of .claude/settings.json ([dbd8506](https://github.com/Kehl-io/nestweaver/commit/dbd8506221b2b9d6829c5d3874428348f820688b))
* collapse nested if-let per clippy ([8f748bb](https://github.com/Kehl-io/nestweaver/commit/8f748bbe4792a7b9af39f74cdd0ae192d8d2e617))
* collapse nested if-let per clippy::collapsible_if ([717434b](https://github.com/Kehl-io/nestweaver/commit/717434b1d2fcc1d71df86c506cb6230ff91a4f16))
* critical bugs batch 2 — daemon fresh-DB, skill generation, UTF-8 panic, write-routing ([188979c](https://github.com/Kehl-io/nestweaver/commit/188979c35de93cefd39f14876f4237fe90b72b4b))
* critical bugs batch 2 — daemon fresh-DB, skill tool count, daemon write-routing ([6c907ab](https://github.com/Kehl-io/nestweaver/commit/6c907aba452e1976e2c0dcf561fbf6d53821ddbe))
* daemon correctness, search parity, investigate fidelity, and agentic integration improvements ([15abc35](https://github.com/Kehl-io/nestweaver/commit/15abc35ced87fdcb1f29499672b542878390ac16))
* **daemon:** catch SIGTERM for graceful shutdown with socket/pidfile cleanup ([54e6346](https://github.com/Kehl-io/nestweaver/commit/54e63466c74da42281bd80e21893ba9dcfdec822))
* **daemon:** decrement active_connections on early return in materialize_projects, restore dist ([42878f8](https://github.com/Kehl-io/nestweaver/commit/42878f8cb5db1ca09a938113c36cdca6f04a3f73))
* **daemon:** derive socket path from stable per-user dir, not $TMPDIR ([#74](https://github.com/Kehl-io/nestweaver/issues/74)) ([40ad9b0](https://github.com/Kehl-io/nestweaver/commit/40ad9b08335197d12eee9949271443666d7a0da3))
* **daemon:** detect stale daemon via DB mtime and auto-restart on connect ([f830f73](https://github.com/Kehl-io/nestweaver/commit/f830f735704e087f1b1f39e7f65453249cf1025f))
* **daemon:** graceful shutdown via signal channel, open DB with write access, spawn_blocking for dispatch ([3313205](https://github.com/Kehl-io/nestweaver/commit/3313205f9c65768721f68efb070e7be698188c03))
* **daemon:** route `brain reindex-search` through daemon RPC ([250a89f](https://github.com/Kehl-io/nestweaver/commit/250a89fad978c91a009aae75fc5e1c87c2db3813))
* **daemon:** serialize env-mutating lifecycle tests to prevent flaky failures ([e969297](https://github.com/Kehl-io/nestweaver/commit/e969297b37c324ce130f42f38bc7f29cf5835621))
* **daemon:** shorten socket path to fit macOS 104-byte sun_path limit ([c26ec2c](https://github.com/Kehl-io/nestweaver/commit/c26ec2c677f02421998713c4f8ef46f17eca0c8a))
* **daemon:** use _with_store index variants to avoid DB lock re-acquisition ([c2382d9](https://github.com/Kehl-io/nestweaver/commit/c2382d99c8538938a8b47bf78cf5041b06040308))
* **daemon:** wrap unary handlers in spawn_blocking, add active_connections tracking, include discarded notes count ([81d3339](https://github.com/Kehl-io/nestweaver/commit/81d3339c2f1db75fecf30c7b481d238e85f1608a))
* derive tool documentation from registry instead of hardcoded tables ([07082fc](https://github.com/Kehl-io/nestweaver/commit/07082fc6543665ff6b1e72f1dc6fd71147db29ad))
* **e2e:** don't toggle settings between view switches — flyout stays open ([54accfc](https://github.com/Kehl-io/nestweaver/commit/54accfcc23394fedce5e174bed47b67ae74d2589))
* **e2e:** fix settings flyout interactions — close after toggle, remove stale Export/Filter button refs ([41d2fd4](https://github.com/Kehl-io/nestweaver/commit/41d2fd40d57b8568d0d4e3c479f7d45135557a46))
* **e2e:** force-click view buttons in settings flyout to bypass visibility check ([dd6643a](https://github.com/Kehl-io/nestweaver/commit/dd6643a8b69b83041836356ffee9f5eee260a07e))
* **e2e:** wait for settings flyout visibility before clicking view buttons ([8c2ddb6](https://github.com/Kehl-io/nestweaver/commit/8c2ddb6956bd73b2719c026089954732ab30068a))
* **embed:** use rustls instead of native-tls to fix cross-compilation ([76b864a](https://github.com/Kehl-io/nestweaver/commit/76b864ac923a2a2865e9a325fff44738b6926e89))
* **engine,cli:** three minor gaps from v0.21.0 adoption validation ([f4fc144](https://github.com/Kehl-io/nestweaver/commit/f4fc144499cfe30fb2ebf5979710bec78aca4c5f))
* **engine:** correct contract_drift boundary check, use store lookup for tag titles ([68faa56](https://github.com/Kehl-io/nestweaver/commit/68faa56c34c6e36395fee59924ca15effb1cfe88))
* **engine:** prevent double-concatenation of Spring/NestJS route prefixes in contract_drift ([c25bd34](https://github.com/Kehl-io/nestweaver/commit/c25bd3463e01ffeade10d74f0ad824fc6c111f79))
* **engine:** RepoConfig.name resolves repo aliases for projects and features ([cf72417](https://github.com/Kehl-io/nestweaver/commit/cf72417d8c21253b6da48685b6d1b3c5cca61934))
* **engine:** resolve tag titles in brain_context instead of showing raw UIDs ([f37783b](https://github.com/Kehl-io/nestweaver/commit/f37783b2a53047a7da6d9194e7a6b771c7ee016d))
* **engine:** route daemon writer-mode Tantivy index into BrainWatcher ([2671758](https://github.com/Kehl-io/nestweaver/commit/2671758f09c76df4d2a89403511b56c5d69925b9))
* **graph:** render ExportMenu inline inside settings flyout to prevent overlay ([4a07e1f](https://github.com/Kehl-io/nestweaver/commit/4a07e1fef77178b73fb00650a35e2e7fb27c03e0))
* **graph:** review fixes — open-source-file path, context menu dismiss, button types ([a8089d3](https://github.com/Kehl-io/nestweaver/commit/a8089d3dbed67c23cf4251d6b90d275dae7c7730))
* **graph:** show fallback node info when API detail unavailable ([49efe7b](https://github.com/Kehl-io/nestweaver/commit/49efe7b79e2c0db96aeb320f6d1b0cc26f6684ab))
* **graph:** wire right-click context menu via store, fix Escape handler conflict ([5565587](https://github.com/Kehl-io/nestweaver/commit/5565587023896a730d25a630ca269968a3f74fb2))
* **grpc:** increase message size limit to 64MB for large responses (dead_code, clusters) ([3fd20d1](https://github.com/Kehl-io/nestweaver/commit/3fd20d12225edfd74449224acc3f9d064ef1f670))
* hash-based instance ID to prevent socket path collisions and length overflow ([3352db5](https://github.com/Kehl-io/nestweaver/commit/3352db5076a927240defb6977ee5863e418e1bd0))
* hide deprecated --allow-writes flag from setup CLI help ([89c1a1b](https://github.com/Kehl-io/nestweaver/commit/89c1a1b4601a71d11c4fdc5c9a4afda201004d9c))
* implement real Elixir type extraction and re-enable HCL tree-sitter ([6df111b](https://github.com/Kehl-io/nestweaver/commit/6df111b1f20c6e39524a45c249b7f61e00151f1a))
* **index:** resolve git HEAD SHA instead of hardcoding 'local' ([9c9ba18](https://github.com/Kehl-io/nestweaver/commit/9c9ba18772b8ba74d04ac83e99a223493c7f6293))
* **mcp:** brain_search concise mode, stale_check commits behind, pagination limits ([cc3a3eb](https://github.com/Kehl-io/nestweaver/commit/cc3a3eb16c8a1cf85dba73fdc8402b80425a7eaf))
* **mcp:** centralize limit default, fix inner-class flow_trace in tools ([b338150](https://github.com/Kehl-io/nestweaver/commit/b33815083e25b3853bf0d27526007c694fefd549))
* **mcp:** collapse nested if-let blocks to satisfy clippy collapsible_if ([002db8c](https://github.com/Kehl-io/nestweaver/commit/002db8cdf6f6d5efa8e5d5856c8ae93af22175a0))
* **mcp:** compute staleness_commits_behind in stale_check, add limit to broken_links and orphan_documents ([836dfc5](https://github.com/Kehl-io/nestweaver/commit/836dfc57136ca37dc36b47c729667e98e8d1b305))
* **mcp:** fix flow_trace class expansion, add pagination to 8 tools, slug-tolerant note_get ([75077e1](https://github.com/Kehl-io/nestweaver/commit/75077e1f901844baf7a75c4e6bcead8b4d55a677))
* **mcp:** log cache serialization failures instead of silently skipping ([c63d0ca](https://github.com/Kehl-io/nestweaver/commit/c63d0ca5c87207223f8beca7800f0bf3337e02ef))
* **mcp:** log instance config discovery, add automated tests for wired limits ([746ffba](https://github.com/Kehl-io/nestweaver/commit/746ffba85a0b8306b05bc3b324fc05099ebf1e77))
* **mcp:** propagate store errors in brain_status instead of returning empty ([2603194](https://github.com/Kehl-io/nestweaver/commit/2603194e19ca0c85e478bb0bb50a17637c85a78a))
* **mcp:** resolve ambiguous symbol names and repo display-name filter ([539172d](https://github.com/Kehl-io/nestweaver/commit/539172df046e8792392dbae96d6a62d53ad65b73))
* **mcp:** resolve vault display names in vaults filter ([bd72968](https://github.com/Kehl-io/nestweaver/commit/bd72968ecb8deaac2b4ee1ea6713469c968441f6))
* **mcp:** respect concise mode in daemon search proxy, add note location, fix tag title fallback ([61c240f](https://github.com/Kehl-io/nestweaver/commit/61c240fa175298f5740fd21a87b410a242a3b8f3))
* **mcp:** route brain_status through JSON pass-through RPC ([61924f4](https://github.com/Kehl-io/nestweaver/commit/61924f497f74f1ac55baa3fadb4bb334d41b8b61))
* **mcp:** slug-tolerant backlinks lookup, hub_nodes clustering hint, regex_search truncated clarity ([a9bb6f9](https://github.com/Kehl-io/nestweaver/commit/a9bb6f9a21bf0fcea0b6a1020056aec2215c5c67))
* **mcp:** use stable state_dir for daemon socket path, not $TMPDIR ([050acef](https://github.com/Kehl-io/nestweaver/commit/050acef826834aebadff48e3904f919df11dfcd1))
* **mcp:** use stable state_dir for daemon socket path, not $TMPDIR ([aca140d](https://github.com/Kehl-io/nestweaver/commit/aca140ddfc14b39ac1a9b0753eca07811c6b498e))
* normalize repo URLs and update docs for v1.1 ([#91](https://github.com/Kehl-io/nestweaver/issues/91)) ([3015771](https://github.com/Kehl-io/nestweaver/commit/30157718c4e12cc02691da993994df0c35b79339))
* **project-context:** surface member symbols, not just notes ([dc95acf](https://github.com/Kehl-io/nestweaver/commit/dc95acfcb8ca25fecdb766af733d7b33e3c41e59))
* **proto:** add instance_id to IndexVaultRequest, thread through daemon and MCP ([8229557](https://github.com/Kehl-io/nestweaver/commit/8229557e350023e10c5cab10818aeb937124619d))
* **proto:** mark semantically-optional proto3 fields as optional ([af67aa0](https://github.com/Kehl-io/nestweaver/commit/af67aa01dd6615f75e8799338e52f88b7408a765))
* **purge-instance:** sweep orphan symbol/file/note rows by UID prefix ([6dfe8be](https://github.com/Kehl-io/nestweaver/commit/6dfe8be7a1520d5d543acfc962256074487129ea))
* reduce baseScale to 0.45 to compensate for larger hard-edge circle ([21a44e5](https://github.com/Kehl-io/nestweaver/commit/21a44e57273ec2d164157bfb2847f55125f59261))
* regex line numbers + install-hook dry-run delta + multi-handler coverage (QA) ([6b1f74d](https://github.com/Kehl-io/nestweaver/commit/6b1f74d8bdc895c908325b1e1ddb6b94dc7fafcb))
* remove duplicate PID write, fix brain_add_source for plain markdown dirs, implement daemon restart ([885fb0d](https://github.com/Kehl-io/nestweaver/commit/885fb0dc402821657134516fc11b2be687a99e73))
* remove stale --allow-mcp-add-sources references from tool descriptions and setup configs ([c65fa9b](https://github.com/Kehl-io/nestweaver/commit/c65fa9b5a26f9b8c09aaee272365ab43da4c2924))
* remove unused variables in autostart error path ([8da6ce4](https://github.com/Kehl-io/nestweaver/commit/8da6ce402c3968d8fc568c84b08b285eb6db02c8))
* resolve 6 reported bugs — lock warning, intent, interactions, merge, seed ranking, limit ([f2be2c2](https://github.com/Kehl-io/nestweaver/commit/f2be2c2f45e2ea97635808242c3964cda8cfe7a1))
* resolve all clippy warnings in resolver crate ([daf3e95](https://github.com/Kehl-io/nestweaver/commit/daf3e954df766a5e226026d8846648e73c9f9d3c))
* resolve clippy warnings and remove dead regex parser files ([e7ffad2](https://github.com/Kehl-io/nestweaver/commit/e7ffad228da3294c30c1c30e421df3098108dae2))
* resolve db from --config on brain read commands (Bug [#19](https://github.com/Kehl-io/nestweaver/issues/19)) ([9a6d56a](https://github.com/Kehl-io/nestweaver/commit/9a6d56a15d35c1459345056c04d3b6f5d70448cf))
* **resolver:** deterministic import resolution — prefer shortest path on ambiguous matches ([50470cb](https://github.com/Kehl-io/nestweaver/commit/50470cbc2f7a198d2a61ad17d3fd876ebe134b08))
* restore base scale factor (0.62) for node sizing ([d16119a](https://github.com/Kehl-io/nestweaver/commit/d16119a37b53574f6d7175634cfdcf0ca48d463e))
* restore lock-error semantics in open_store, cascade-delete vault notes during merge ([a1dcc3b](https://github.com/Kehl-io/nestweaver/commit/a1dcc3b1e4e8676f0efa3af808a7f9de5c8a5b03))
* revert HCL to regex parser, fix Swift types query, relax Julia/PowerShell tests ([92058c5](https://github.com/Kehl-io/nestweaver/commit/92058c59277c71ef54f38cf3afe88a49b3bfeca3))
* route CLI read commands through daemon (fixes [#63](https://github.com/Kehl-io/nestweaver/issues/63)) ([691591a](https://github.com/Kehl-io/nestweaver/commit/691591aff17d1b4d58015b3d51b71d42a7592f87))
* rustfmt formatting ([0adcbe6](https://github.com/Kehl-io/nestweaver/commit/0adcbe6cd78f892c01e86445144596b3ad1e5b1d))
* rustfmt formatting ([ba90193](https://github.com/Kehl-io/nestweaver/commit/ba90193b92ddeb0f21f8408e76a6caa591ce5846))
* rustfmt formatting for new parser modules ([e8fb8dc](https://github.com/Kehl-io/nestweaver/commit/e8fb8dc72506bbbc49a41f77b801b881db867f98))
* rustfmt imports.rs ([d034cf6](https://github.com/Kehl-io/nestweaver/commit/d034cf620e2c4c33f7bea67cc4aee6c376146290))
* **search:** treat `limit` as per-kind so symbols stop being squeezed out ([ad1be37](https://github.com/Kehl-io/nestweaver/commit/ad1be376581c653b15d886e1cda8e809c208515b))
* **setup:** protect existing skill/rule files from overwrite ([cb4326c](https://github.com/Kehl-io/nestweaver/commit/cb4326c486cb60ac9de55cbceb7379877841deb4))
* **snapshot:** decouple min_compatible_engine from build version ([2845d41](https://github.com/Kehl-io/nestweaver/commit/2845d41950915d05490c90cc76ef0e3b308e3995))
* **snapshot:** query actual embedding dimension from store ([cc09507](https://github.com/Kehl-io/nestweaver/commit/cc0950795dbfffbb6faaa2ba9042e963bed4599c))
* **storage:** LocalBackend push/pull now copies subdirectories ([30a651d](https://github.com/Kehl-io/nestweaver/commit/30a651d5ebb2e5e5072ae3a2cb6b68601e40b6d6))
* **store:** add members_of() query and fix inner-class flow_trace scoping ([dc7963d](https://github.com/Kehl-io/nestweaver/commit/dc7963d68ca97e7176f2f82bc8ef8898538b6212))
* **store:** address review findings — transaction safety, ordering, cache version ([49f9727](https://github.com/Kehl-io/nestweaver/commit/49f972707652f24fdd97173b230a00cc7287ef62))
* **store:** deduplicate vaults during instance merge ([bc97ad5](https://github.com/Kehl-io/nestweaver/commit/bc97ad56e3ac31f18d786cb10a4b24f6cfacbae5))
* **store:** hash vec lengths in scope_hash to prevent prefix collisions ([037ed93](https://github.com/Kehl-io/nestweaver/commit/037ed935ab0bffece45db81603110c787d87125a))
* **store:** make insert_vault idempotent and remove obsolete binary ([25192ca](https://github.com/Kehl-io/nestweaver/commit/25192ca173cc46aa7dee19c6d9f6ae45f75f9ac2))
* **store:** prefer populated vault on collision merge, warn about unlinked notes ([c49e67d](https://github.com/Kehl-io/nestweaver/commit/c49e67d0d0f6974848fc0984b80733f7b4a68a09))
* **store:** preserve SECTION_TAGGED_WITH edges during vault reparent ([7fb02c5](https://github.com/Kehl-io/nestweaver/commit/7fb02c5df27e62349f2ed4e7e9e057be059191b3))
* **store:** prevent data loss in merge_instance_ids via reparent_vault ([2b46f35](https://github.com/Kehl-io/nestweaver/commit/2b46f3583db1449af323211804a84c0084ad2a7e))
* **store:** safe vault UID rewrite during instance merge, add --limit docs ([dffb1f7](https://github.com/Kehl-io/nestweaver/commit/dffb1f7ec2f13ffa860f9e12b80a1569daca331d))
* **store:** use DETACH DELETE pattern in merge_instance_ids ([bfa0568](https://github.com/Kehl-io/nestweaver/commit/bfa05686f8f097013b42cf27a00747ec282db50d))
* **store:** use SYMBOL_COLUMNS in update_symbol_embedding query ([48a9fd7](https://github.com/Kehl-io/nestweaver/commit/48a9fd710efdf1284a1d2959289f0c35df99d6e0))
* **store:** use upsert_vault with DETACH DELETE pattern ([c393d6f](https://github.com/Kehl-io/nestweaver/commit/c393d6f122e5390d509523b58a30b6b19f8a9d21))
* surface project member notes in project_context (Bug [#12](https://github.com/Kehl-io/nestweaver/issues/12)) ([2b0f94a](https://github.com/Kehl-io/nestweaver/commit/2b0f94a54b0c00811f4d148caecf7db23ba09351))
* **ui:** open store read-write for watch mode, read-only for default UI ([2efaf38](https://github.com/Kehl-io/nestweaver/commit/2efaf383e6ecb2b24697ed51d2dadbf6d4c94d4a))
* update stale daemonize references to daemonize2 in comments ([f60ea80](https://github.com/Kehl-io/nestweaver/commit/f60ea8041b5695fc248ca37064be7b0994555b2e))
* use multiplicative confidence decay instead of linear (research-backed) ([a87ca53](https://github.com/Kehl-io/nestweaver/commit/a87ca532376da61dfb7ac15889bb97379f6fa498))
* use TypedEdge type alias to satisfy clippy complexity lint ([4cc0c47](https://github.com/Kehl-io/nestweaver/commit/4cc0c47681bfa47a9aa3514f10c970ac5b115e32))
* UTF-8 char-boundary panic in type extractors + daemon fresh-DB canonicalize ([73f6b7f](https://github.com/Kehl-io/nestweaver/commit/73f6b7f70cd47722db2a27c42df8e5c7174086d9))
* **v0.21.0:** regression fixes, adoption validation, and performance ([e37ab2a](https://github.com/Kehl-io/nestweaver/commit/e37ab2a57216565c29b50ad57090545003476bc9))
* **v0.22.0:** instance merge dataloss, interaction tracking, daemon parity, edge-type centralization ([18d52df](https://github.com/Kehl-io/nestweaver/commit/18d52dfefd478f435dd8d1140cb8e44173d35562))
* vault idempotency, stale-check CLI, instance merge, and CI linker fix ([43d13e7](https://github.com/Kehl-io/nestweaver/commit/43d13e7d68c5a3467620e174588ea2d86f6742be))
* warn on deprecated --allow-mcp-add-sources and strip from existing configs ([c91a310](https://github.com/Kehl-io/nestweaver/commit/c91a310fe96df1ab1e95d083c63b669ab82a718e))
* watcher lock detection, daemon log rotation, actionable error messages ([cdb8845](https://github.com/Kehl-io/nestweaver/commit/cdb884569cdf16eded39af61465c723f1e424976))
* **watcher:** remove stale title→UID mapping on note rename before adding new title ([6e6c088](https://github.com/Kehl-io/nestweaver/commit/6e6c0880b15c3f6695014eaf86106a138335a515))
* **web:** address review findings — brand colors, a11y, dedup, dead code ([39612ac](https://github.com/Kehl-io/nestweaver/commit/39612ac05c50dc30570de7de300009f9acb977fe))
* **web:** allow search dropdown to escape header bounds ([14fb3e7](https://github.com/Kehl-io/nestweaver/commit/14fb3e772bfb916e32837c5f8443eff64c200f37))
* **web:** balance overview landmarks ([994b714](https://github.com/Kehl-io/nestweaver/commit/994b714ad0a897351980991f1eb688d496c1e385))
* **web:** commit AppState Arc&lt;TantivyIndex&gt; refactor (fixes CI build) ([5ea3cf1](https://github.com/Kehl-io/nestweaver/commit/5ea3cf1a5e742263ecbed6822497de5bfdfa777d))
* **web:** gate overview actions by supported targets ([1378075](https://github.com/Kehl-io/nestweaver/commit/1378075d9470710eba853bea008c1505b5b8fbcf))
* **web:** guard overview graph updates ([2fc38df](https://github.com/Kehl-io/nestweaver/commit/2fc38dfbf360023b1c5514a273ae293911d78b79))
* **web:** make overview entry points actionable ([d3d02db](https://github.com/Kehl-io/nestweaver/commit/d3d02db6fa5eee7ef54843653bfcb5e6fb0096d5))
* **web:** migrate persisted graph mode ([8bc0ee2](https://github.com/Kehl-io/nestweaver/commit/8bc0ee2294cd4aa4b961cb39d0f94d9dcbedf234))
* **web:** polish guided overview verification ([2525be2](https://github.com/Kehl-io/nestweaver/commit/2525be2d65b41e1780b3fc51eba743b967e6bc68))
* **web:** preserve overview landmark labels ([26c9dea](https://github.com/Kehl-io/nestweaver/commit/26c9dea44b793a58f22f6476ca4c93f7a95a8a10))
* **web:** preserve repo git suffix in overview ([80690e3](https://github.com/Kehl-io/nestweaver/commit/80690e3de810f2978ff8bb6137f9b65296200489))
* **web:** refine guided overview browser pass ([73e1b66](https://github.com/Kehl-io/nestweaver/commit/73e1b66d1deb3f5dc113c6722fa18660fa0796d6))
* **web:** reset graph mode on page load ([63d4ab6](https://github.com/Kehl-io/nestweaver/commit/63d4ab65f1a90d9a1d86fee159822879a3301563))
* **web:** scope e2e search click to dropdown, not graph label ([3663473](https://github.com/Kehl-io/nestweaver/commit/3663473ce05326d4bfcc6711ad67c7fcd96d8ca5))
* **web:** sharpen graph nodes and labels ([7810b09](https://github.com/Kehl-io/nestweaver/commit/7810b09aa5925b8129d2cc07b6f48cdc2205ca2a))
* **web:** simplify graph nodes to led dots ([ca184b4](https://github.com/Kehl-io/nestweaver/commit/ca184b4b65fe353eb28df534b65a8e864417a595))
* **web:** soften graph node borders ([a6788c4](https://github.com/Kehl-io/nestweaver/commit/a6788c400fb718d6c2cd53cb28c14d0544cea477))


### Performance Improvements

* comprehensive performance audit — 35x index, 10000x cache, parallel resolution ([5005e3f](https://github.com/Kehl-io/nestweaver/commit/5005e3f2603ab1c98d11e5d6f398a174744e3f81))
* **cross-domain:** batch edge commits in single transactions ([d1dec00](https://github.com/Kehl-io/nestweaver/commit/d1dec003dfc26aa49d6d1666af55781d74fdb978))
* **daemon:** skip Tantivy reindex after code repo indexing (Tantivy only indexes notes) ([f957940](https://github.com/Kehl-io/nestweaver/commit/f95794069234c2a518e49e3671eb5d6a3b8244a2))
* **engine,cli,mcp:** dedup Heading/Section pairs + daemon path for project-context ([d0c8f2c](https://github.com/Kehl-io/nestweaver/commit/d0c8f2c62d5d8491f3696465bc4750f50cb2cac8))
* **engine:** parallelize type environment construction with rayon ([9b77e3e](https://github.com/Kehl-io/nestweaver/commit/9b77e3e6586b0df39089dd9adc3d062f45c7d703))
* **engine:** retain parsed source to avoid redundant disk re-reads ([7d318db](https://github.com/Kehl-io/nestweaver/commit/7d318db832674470039486ef8912adec2e9ddd9a))
* **engine:** use bulk delete for force re-index cleanup ([0810c61](https://github.com/Kehl-io/nestweaver/commit/0810c61ab2556e0fd20830120d65b53728d7a97f))
* **index:** defer PageRank computation to first query (lazy evaluation) ([644ee2d](https://github.com/Kehl-io/nestweaver/commit/644ee2d89bf3919ffb6300663756cd99585a3a2c))
* indexing performance, daemon routing, and agent-friendly setup ([#94](https://github.com/Kehl-io/nestweaver/issues/94)) ([c63287b](https://github.com/Kehl-io/nestweaver/commit/c63287b79a21f1a3086f929ffa17ca633ce18716))
* **index:** parallelize markdown note parsing with rayon ([2e6fd19](https://github.com/Kehl-io/nestweaver/commit/2e6fd195e15574fd0e3e25f0c96172dbb7ee9d16))
* **mcp:** hold response cache in-process with periodic flush ([8bf43bb](https://github.com/Kehl-io/nestweaver/commit/8bf43bb9bf3e261b6535616cf14baf8df9c4bca5))
* **pagerank:** add warm-start support for faster convergence after incremental updates ([cefb267](https://github.com/Kehl-io/nestweaver/commit/cefb267733cefea0af7b0ed9d3ff16a5491ccc71))
* **parser:** cache compiled tree-sitter Query objects globally ([ad168ca](https://github.com/Kehl-io/nestweaver/commit/ad168cad36d675a770b0039907b5c8fe35d80b2f))
* remove bloom post-processing, update background to Catppuccin base ([bf03121](https://github.com/Kehl-io/nestweaver/commit/bf03121f91e92495274e5abcb121094cab46e354))
* **resolver:** parallelize per-file reference resolution with rayon ([d74e8de](https://github.com/Kehl-io/nestweaver/commit/d74e8de982b8079f8334a73744436165231aa129))
* **resolver:** use binary search for find_enclosing_symbol (O(n) → O(log n)) ([62c6423](https://github.com/Kehl-io/nestweaver/commit/62c6423ee8291a23923cbb0a0cb3f85cfa515476))
* **store:** add _on variants for markdown batch inserts ([a5eefad](https://github.com/Kehl-io/nestweaver/commit/a5eefad18aebed90271a541fc0097f5ae6c9ebd1))
* **store:** add lookup_tag() for O(1) tag resolution, replace list_tags() scans ([00d9c2b](https://github.com/Kehl-io/nestweaver/commit/00d9c2b9c7da45676cbc2607bbfff3cc27f350cf))
* **store:** batch symbol lookups to eliminate N+1 after PPR ([a28b837](https://github.com/Kehl-io/nestweaver/commit/a28b83717ebe18ad62ee18e62800d4f6c219369d))
* **store:** batch Tantivy commits and use bulk section/heading loaders ([cbdf261](https://github.com/Kehl-io/nestweaver/commit/cbdf2611193037f42e3a4f2190e81aa8c08ab581))
* **store:** bulk DETACH DELETE for file symbols instead of per-UID queries ([11f30b6](https://github.com/Kehl-io/nestweaver/commit/11f30b647b1f3bbf87cdad8ddfcaa972cab45b98))
* **store:** cache PPR adjacency graph keyed on (generation, scope, intent) ([ec9bcf9](https://github.com/Kehl-io/nestweaver/commit/ec9bcf98d288b107bee1b23a9766005542613eb6))
* **store:** cache symbol name index keyed on graph generation ([98e0fa0](https://github.com/Kehl-io/nestweaver/commit/98e0fa0b93ccb9c7e5674b1db3d3c3a5c69e97a2))
* **store:** replace per-row cascade deletes with bulk DETACH DELETE ([bdb04a1](https://github.com/Kehl-io/nestweaver/commit/bdb04a11a8d85f23dccde4be71ac13ddbef425fa))
* **store:** single-query orphan sweep and allocation-free kind_rank ([d6bdb65](https://github.com/Kehl-io/nestweaver/commit/d6bdb6548f3c98810e51fa68f4f4bc028719dd78))
* **store:** switch response cache to binary format (MessagePack + ZSTD) ([27daa3b](https://github.com/Kehl-io/nestweaver/commit/27daa3b8093d4447680970374f23b003abe628a1))
* **store:** wrap vault indexing in single transaction via bulk_vault_write ([4943df3](https://github.com/Kehl-io/nestweaver/commit/4943df35b1d6ad1952250da473e39c7adbc1d783))
* **tantivy:** update search index after daemon indexing operations ([989a441](https://github.com/Kehl-io/nestweaver/commit/989a441a6291e9fa04e66f18f6d81335b402ad66))
* **watcher:** bidirectional map for O(1) wikilink title lookup updates on rename ([d4393bd](https://github.com/Kehl-io/nestweaver/commit/d4393bdbb1ec7d9b6d1a807b6a96f466187d60d4))
* **watcher:** cache wikilink title lookup across batch, avoid per-note list_notes query ([1f3f4d0](https://github.com/Kehl-io/nestweaver/commit/1f3f4d0bbba139e27b9e52ef7f5408785503cd68))

## [1.1.3](https://github.com/Kehl-io/nestweaver/compare/v1.1.2...v1.1.3) (2026-06-22)


### Bug Fixes

* **daemon:** detect stale daemon via DB mtime and auto-restart on connect ([a01badd](https://github.com/Kehl-io/nestweaver/commit/a01badd1ede54437df109ffe9b524d7cca31f235))
* **index:** resolve git HEAD SHA instead of hardcoding 'local' ([37a06dd](https://github.com/Kehl-io/nestweaver/commit/37a06dd65ded6bae4aafc4884b4355bfa4861dba))
* **mcp:** propagate store errors in brain_status instead of returning empty ([b698edb](https://github.com/Kehl-io/nestweaver/commit/b698edb5488e54f79e6309185f04f81b43c6fe76))

## [1.1.2](https://github.com/Kehl-io/nestweaver/compare/v1.1.1...v1.1.2) (2026-06-22)


### Performance Improvements

* indexing performance, daemon routing, and agent-friendly setup ([#94](https://github.com/Kehl-io/nestweaver/issues/94)) ([1f75479](https://github.com/Kehl-io/nestweaver/commit/1f7547904a66236969bd539fe9da52be5c269a4e))

## [1.1.1](https://github.com/Kehl-io/nestweaver/compare/v1.1.0...v1.1.1) (2026-06-21)


### Bug Fixes

* normalize repo URLs and update docs for v1.1 ([#91](https://github.com/Kehl-io/nestweaver/issues/91)) ([4e68c38](https://github.com/Kehl-io/nestweaver/commit/4e68c382496156c71ef77cf30055d31d3025b8d0))

## [1.1.0](https://github.com/Kehl-io/nestweaver/compare/v1.0.1...v1.1.0) (2026-06-20)


### Features

* update dependencies, launchd daemon, and macOS app bundle ([#88](https://github.com/Kehl-io/nestweaver/issues/88)) ([f6c299b](https://github.com/Kehl-io/nestweaver/commit/f6c299b7aec291668af2eb75204c1a3ede90631f))

## [1.0.1](https://github.com/Kehl-io/nestweaver/compare/v1.0.0...v1.0.1) (2026-06-20)


### Bug Fixes

* **embed:** use rustls instead of native-tls to fix cross-compilation ([1092704](https://github.com/Kehl-io/nestweaver/commit/1092704b0f0ef26b133a23f170236c673247f7fc))

## [1.0.0](https://github.com/Kehl-io/nestweaver/compare/v0.28.0...v1.0.0) (2026-06-20)


### ⚠ BREAKING CHANGES

* embedding seed layer — local model semantic search with Metal acceleration ([#82](https://github.com/Kehl-io/nestweaver/issues/82))

### Features

* embedding seed layer — local model semantic search with Metal acceleration ([#82](https://github.com/Kehl-io/nestweaver/issues/82)) ([eceb68d](https://github.com/Kehl-io/nestweaver/commit/eceb68da3f685d4e1d71a8e900397506a073beab))

## [0.28.0](https://github.com/Kehl-io/nestweaver/compare/v0.27.0...v0.28.0) (2026-06-18)


### Features

* **daemon:** eliminate all stop_daemon_if_running calls ([ec6d2c2](https://github.com/Kehl-io/nestweaver/commit/ec6d2c27ff5e6d50e25099f45aacb4528bb9fa04))
* enrich brain_guide/admin instructions with tool-routing table, add staleness warnings and token hints ([f5db6f7](https://github.com/Kehl-io/nestweaver/commit/f5db6f7d6e4590fa9df9869c26c98c5fd6c8d026))


### Bug Fixes

* bump gRPC message size limit from 64MB to 256MB (fixes export cypher) ([6306a48](https://github.com/Kehl-io/nestweaver/commit/6306a48a96224cf2398cbbdb090d7b81a3010d5d))
* **ci:** set NESTWEAVER_NO_DAEMON=1 for E2E tests ([2cb77a4](https://github.com/Kehl-io/nestweaver/commit/2cb77a497ca005864e1881232c7fa6aa2c011ee1))
* **cli:** add daemon RPCs for list-repos/services/projects/search/symbol ([55f2b50](https://github.com/Kehl-io/nestweaver/commit/55f2b507f966ac435aff73a7f9d7e42c6c7761a0))
* **cli:** graceful daemon fallback for ui/watch, fix collapsible-if clippy warnings ([9790a5e](https://github.com/Kehl-io/nestweaver/commit/9790a5eb62336717021e1276eaef3692c3757e2e))
* **cli:** guard --no-daemon for testing, route brain-remove/snapshot through daemon ([6450281](https://github.com/Kehl-io/nestweaver/commit/645028128aa4267ac0821462d3c81778a9684791))
* **cli:** guard --no-daemon in MCP server command consistently ([32f8472](https://github.com/Kehl-io/nestweaver/commit/32f8472fd80ee719eaafdcfc9fb87777aa150c0a))
* **cli:** route 10 read commands through daemon, improve lock error message ([5486618](https://github.com/Kehl-io/nestweaver/commit/5486618d838838be03fc0babd4bbcebfa34952de)), closes [#63](https://github.com/Kehl-io/nestweaver/issues/63)
* **cli:** route all 5 json-gated commands through daemon unconditionally ([8cb5152](https://github.com/Kehl-io/nestweaver/commit/8cb5152609cd0d907545031f9c8862814190ae86))
* **cli:** route context/cross-repo-refs/generate-guide/cluster through daemon ([7f8377a](https://github.com/Kehl-io/nestweaver/commit/7f8377af3c55811a1fb047406437f9c99e8dfede))
* **cli:** route export and ui commands through daemon stop/restart ([9063194](https://github.com/Kehl-io/nestweaver/commit/9063194cda6f38e65c96ad9f689edefd48e6b0bf))
* **cli:** route remove-repo/remove-project resolution through daemon RPCs ([800f549](https://github.com/Kehl-io/nestweaver/commit/800f549be3cffcd5434de2fa5e371c5559fcdc0e))
* **cli:** route repo-map/suggest-links/detect-implicit-projects/pr-impact through daemon ([8a57a4c](https://github.com/Kehl-io/nestweaver/commit/8a57a4c84f0cf29e0e9f99777e437794401ccafd))
* **cli:** route watch/export/ui through daemon (zero bypasses remaining) ([4bda18b](https://github.com/Kehl-io/nestweaver/commit/4bda18bc0ca3da9afaab93461cbe51a83e50d9a5))
* route CLI read commands through daemon (fixes [#63](https://github.com/Kehl-io/nestweaver/issues/63)) ([d71c53f](https://github.com/Kehl-io/nestweaver/commit/d71c53f7abeec61e743c342ee0b69530afc0bf72))
* **web:** commit AppState Arc&lt;TantivyIndex&gt; refactor (fixes CI build) ([6274f31](https://github.com/Kehl-io/nestweaver/commit/6274f31ef70653037de4e19d342823b9014ed888))

## [0.27.0](https://github.com/Kehl-io/nestweaver/compare/v0.26.3...v0.27.0) (2026-06-17)


### Features

* add remove-repo CLI command and daemon RPC ([f9f0bac](https://github.com/Kehl-io/nestweaver/commit/f9f0bac5b1771f3438fa85df3fa5e78eac519e43))
* **cli:** add remove-project and prune-stale commands ([cc7682a](https://github.com/Kehl-io/nestweaver/commit/cc7682abaca500bb74e20c88bdf9981352fa27eb))
* **cli:** add remove-repo command ([f0ad081](https://github.com/Kehl-io/nestweaver/commit/f0ad081aeaa6c91f1a05b8c824347ca96d35e19c))
* **client:** add remove_project and prune_stale methods ([0555361](https://github.com/Kehl-io/nestweaver/commit/05553616a6155879b585e4141da7902ad1ff61de))
* **client:** add remove_repo method ([1e1d8db](https://github.com/Kehl-io/nestweaver/commit/1e1d8dba04d24fbe54756764a8a856d5e29ad368))
* **daemon:** implement RemoveProject and PruneStale handlers ([7723612](https://github.com/Kehl-io/nestweaver/commit/772361257d648cdd7e9de0e757aa3017022761d4))
* **daemon:** implement RemoveRepo RPC handler ([29c3fdc](https://github.com/Kehl-io/nestweaver/commit/29c3fdcfd81b53cfcd7a636d31ae97c2e3cd0c45))
* **mcp:** add brain_remove_source and prune_stale tools ([f46ca49](https://github.com/Kehl-io/nestweaver/commit/f46ca49e6b2797da22913392a80d3662abfb249e))
* **proto:** add RemoveProject and PruneStale RPCs ([71a4126](https://github.com/Kehl-io/nestweaver/commit/71a412659a8bfc8555db342bd07164580608a3ce))
* **proto:** add RemoveRepo RPC message and service method ([5f72262](https://github.com/Kehl-io/nestweaver/commit/5f72262c1d0ad213f3e3bf054d91192ea1a7d0cd))


### Bug Fixes

* clippy/fmt fixes + docs for remove-project, prune-stale, brain_remove_source ([020254b](https://github.com/Kehl-io/nestweaver/commit/020254b0a7235aa473eb0c38489adaa62a1c7164))

## [0.26.3](https://github.com/Kehl-io/nestweaver/compare/v0.26.2...v0.26.3) (2026-06-17)


### Bug Fixes

* **mcp:** use stable state_dir for daemon socket path, not $TMPDIR ([6813314](https://github.com/Kehl-io/nestweaver/commit/6813314fa8f7eef6f722754eba8a5ae1f72b5032))
* **mcp:** use stable state_dir for daemon socket path, not $TMPDIR ([3a1edf6](https://github.com/Kehl-io/nestweaver/commit/3a1edf6476b23813559f5e288d116adf866ede90))

## [0.26.2](https://github.com/Kehl-io/nestweaver/compare/v0.26.1...v0.26.2) (2026-06-17)


### Bug Fixes

* **daemon:** derive socket path from stable per-user dir, not $TMPDIR ([#74](https://github.com/Kehl-io/nestweaver/issues/74)) ([2febd6e](https://github.com/Kehl-io/nestweaver/commit/2febd6ee1095b4c04f127d4aaa944cb6b2cf8409))

## [0.26.1](https://github.com/Kehl-io/nestweaver/compare/v0.26.0...v0.26.1) (2026-06-17)


### Bug Fixes

* **cli:** add --limit to broken-links, orphans, topic-clusters, tag-graph; surface staleness_commits_behind ([6971396](https://github.com/Kehl-io/nestweaver/commit/6971396a31fac3e8e81df16e9f5821173ee5c338))
* **cli:** add --limit to broken-links, orphans, topic-clusters, tag-graph; surface staleness_commits_behind ([39b5ba5](https://github.com/Kehl-io/nestweaver/commit/39b5ba5892784b14df025231e259e1731b11334e))

## [0.26.0](https://github.com/Kehl-io/nestweaver/compare/v0.25.1...v0.26.0) (2026-06-16)


### Features

* **cli:** wire [limits].default_result_limit to CLI search and context commands ([d804176](https://github.com/Kehl-io/nestweaver/commit/d804176a9611fbdeae9610d8da0006a616f510a9))
* **config:** wire [limits].default_result_limit to runtime tool dispatch ([814c4bd](https://github.com/Kehl-io/nestweaver/commit/814c4bdd9016a9939bda0990bb01039e9eafe1b2))
* wire [limits].default_result_limit to runtime dispatch ([40a5c80](https://github.com/Kehl-io/nestweaver/commit/40a5c80ba1f3cc6c1a42dd568605bb133e456b75))


### Bug Fixes

* **mcp:** log instance config discovery, add automated tests for wired limits ([37e49da](https://github.com/Kehl-io/nestweaver/commit/37e49da5b6fc7947df73aa1c67f4351ea2d53c77))

## [0.25.1](https://github.com/Kehl-io/nestweaver/compare/v0.25.0...v0.25.1) (2026-06-16)


### Bug Fixes

* address code review findings for production readiness ([4ac2627](https://github.com/Kehl-io/nestweaver/commit/4ac262714641557675bc80e24802bbcb698ffd86))
* **daemon:** serialize env-mutating lifecycle tests to prevent flaky failures ([ed3eed3](https://github.com/Kehl-io/nestweaver/commit/ed3eed3e8e2b2964b502fa6004dc693f1b1025d4))
* **engine:** correct contract_drift boundary check, use store lookup for tag titles ([d1f034d](https://github.com/Kehl-io/nestweaver/commit/d1f034d28fb1d253706558d0f15ef07be9a9d9c5))
* **engine:** prevent double-concatenation of Spring/NestJS route prefixes in contract_drift ([c8110c8](https://github.com/Kehl-io/nestweaver/commit/c8110c8d873dc409113223819c7437ec6ddd4e5f))
* **engine:** resolve tag titles in brain_context instead of showing raw UIDs ([ac4c3a6](https://github.com/Kehl-io/nestweaver/commit/ac4c3a6cbd1702cadad08cdaed3a18fe88e40364))
* **mcp:** brain_search concise mode, stale_check commits behind, pagination limits ([b6db270](https://github.com/Kehl-io/nestweaver/commit/b6db270d50b7b86882eea90e4dd1b1ddba74d68f))
* **mcp:** centralize limit default, fix inner-class flow_trace in tools ([27b53ff](https://github.com/Kehl-io/nestweaver/commit/27b53ff192d7acb0cac6fd545daf8f71f138c3c8))
* **mcp:** collapse nested if-let blocks to satisfy clippy collapsible_if ([6cbcb75](https://github.com/Kehl-io/nestweaver/commit/6cbcb75476eaf377a87c66a847745faf6f5fa982))
* **mcp:** compute staleness_commits_behind in stale_check, add limit to broken_links and orphan_documents ([5bab151](https://github.com/Kehl-io/nestweaver/commit/5bab15128d201a9dde68acf7a32b130280e57ffb))
* **mcp:** fix flow_trace class expansion, add pagination to 8 tools, slug-tolerant note_get ([d796686](https://github.com/Kehl-io/nestweaver/commit/d79668623fefbc8326042aa60df231d5f2cb7938))
* **mcp:** respect concise mode in daemon search proxy, add note location, fix tag title fallback ([98ce6cc](https://github.com/Kehl-io/nestweaver/commit/98ce6cc5165f74afa0a1b75fc1808524be8a71db))
* **mcp:** slug-tolerant backlinks lookup, hub_nodes clustering hint, regex_search truncated clarity ([9b57f66](https://github.com/Kehl-io/nestweaver/commit/9b57f6642604b74ce8b7c12ded908c2b1ce0ddd8))
* **proto:** mark semantically-optional proto3 fields as optional ([e2b59a9](https://github.com/Kehl-io/nestweaver/commit/e2b59a9aa71116492f417b7cc1851d7c33384c6d))
* **store:** add members_of() query and fix inner-class flow_trace scoping ([f1d0f5c](https://github.com/Kehl-io/nestweaver/commit/f1d0f5c812f4ee647b6fb0d3f3cdae7dafed1a3a))


### Performance Improvements

* **store:** add lookup_tag() for O(1) tag resolution, replace list_tags() scans ([1b9f15b](https://github.com/Kehl-io/nestweaver/commit/1b9f15b0bffee59ee25cc820875a8a011c962fe0))

## [0.25.0](https://github.com/Kehl-io/nestweaver/compare/v0.24.0...v0.25.0) (2026-06-16)


### Features

* **client:** add materialize_projects, remove_vault, merge_instance, purge_instance methods ([952cd35](https://github.com/Kehl-io/nestweaver/commit/952cd35cd50546b43ee3f1df8c3b05946df72dfb))
* **daemon:** implement MaterializeProjects, RemoveVault, MergeInstance, PurgeInstance handlers ([3d17b1b](https://github.com/Kehl-io/nestweaver/commit/3d17b1b6b4ff5267e16ee3139e37eceff7293914))
* **proto:** add MaterializeProjects, RemoveVault, MergeInstance, PurgeInstance RPCs ([9c9e10d](https://github.com/Kehl-io/nestweaver/commit/9c9e10de2bac7b39f13a49a56a21298166f3cabe))
* route all database writes through daemon RPC ([ed2396d](https://github.com/Kehl-io/nestweaver/commit/ed2396d6ee54caa2013f8d698c095834fedccfad))


### Bug Fixes

* **cli:** keep direct-write fallback for index/brain-add when NESTWEAVER_NO_DAEMON=1 ([aa0ae50](https://github.com/Kehl-io/nestweaver/commit/aa0ae5084ffe3ace0aede3c6a74ea6f52606fc8c))
* **daemon:** decrement active_connections on early return in materialize_projects, restore dist ([575eae8](https://github.com/Kehl-io/nestweaver/commit/575eae888ff71264793e19b7fed45657b00e79ff))
* **daemon:** wrap unary handlers in spawn_blocking, add active_connections tracking, include discarded notes count ([6cf7298](https://github.com/Kehl-io/nestweaver/commit/6cf72981775fa628192344ac32653e492e1d9096))
* **ui:** open store read-write for watch mode, read-only for default UI ([5fc082d](https://github.com/Kehl-io/nestweaver/commit/5fc082d5a2412942ca18cc5da573d591fc445135))

## [0.24.0](https://github.com/Kehl-io/nestweaver/compare/v0.23.0...v0.24.0) (2026-06-16)


### Features

* **graph:** add node preview card component ([a6ea76f](https://github.com/Kehl-io/nestweaver/commit/a6ea76f61084dfbb25b0f2865d90d368bed86939))
* **graph:** add preview card state to store ([c67e851](https://github.com/Kehl-io/nestweaver/commit/c67e85144e557056fd2ecb733c13901c44392338))
* **graph:** add useNodePreview hook with LRU cache ([66b378e](https://github.com/Kehl-io/nestweaver/commit/66b378e5aac41559fe5a4043e8aa057041413ed3))
* **graph:** Obsidian-style visual reskin + click-to-preview UX ([e33eaf5](https://github.com/Kehl-io/nestweaver/commit/e33eaf5a2de5b370e36a62dafe0d27aa1f27c4f0))
* **graph:** populate context menu with grouped power actions ([5ff9e7a](https://github.com/Kehl-io/nestweaver/commit/5ff9e7a1295754742dfc79bcfc0eccf545392a5f))
* **graph:** wire click-to-preview and escape dismiss ([cd34cea](https://github.com/Kehl-io/nestweaver/commit/cd34cea1968b580c8bc70d19de0aeb4359c33f37))


### Bug Fixes

* **e2e:** don't toggle settings between view switches — flyout stays open ([3ac9efb](https://github.com/Kehl-io/nestweaver/commit/3ac9efb0e7fd77d0aaf6f5f55c95efefb761e6f2))
* **e2e:** fix settings flyout interactions — close after toggle, remove stale Export/Filter button refs ([0eef514](https://github.com/Kehl-io/nestweaver/commit/0eef5141f06f3550a398e376b1bb1ab3e191d21c))
* **e2e:** force-click view buttons in settings flyout to bypass visibility check ([966e3f9](https://github.com/Kehl-io/nestweaver/commit/966e3f95eae9eacbe232cc8281af2e5815bf182c))
* **e2e:** wait for settings flyout visibility before clicking view buttons ([7946460](https://github.com/Kehl-io/nestweaver/commit/7946460b790b26725c47dbd1847399ea66efd3ae))
* **graph:** render ExportMenu inline inside settings flyout to prevent overlay ([50f3ce5](https://github.com/Kehl-io/nestweaver/commit/50f3ce5c701c35e1d498dd63aa5e1420fc96d344))
* **graph:** review fixes — open-source-file path, context menu dismiss, button types ([c39effe](https://github.com/Kehl-io/nestweaver/commit/c39effe6b225d51a48449b06514c24bef9715a65))
* **graph:** show fallback node info when API detail unavailable ([4f08f9f](https://github.com/Kehl-io/nestweaver/commit/4f08f9fae1267e9c9ccb05af0d264789d05d0fc5))
* **graph:** wire right-click context menu via store, fix Escape handler conflict ([0d57c38](https://github.com/Kehl-io/nestweaver/commit/0d57c38bb2126bc8bae0632a7d8f72303975960f))
* reduce baseScale to 0.45 to compensate for larger hard-edge circle ([2328926](https://github.com/Kehl-io/nestweaver/commit/23289260997203583b37b07aeed320683a971578))
* restore base scale factor (0.62) for node sizing ([2bcf49c](https://github.com/Kehl-io/nestweaver/commit/2bcf49c2d8710c63f7e62d4a4b88b571ffc7b246))


### Performance Improvements

* remove bloom post-processing, update background to Catppuccin base ([d5a8525](https://github.com/Kehl-io/nestweaver/commit/d5a8525876f9f32db4062343c1a92560393d825c))

## [0.23.0](https://github.com/Kehl-io/nestweaver/compare/v0.22.0...v0.23.0) (2026-06-13)


### Features

* **cli:** accept --track-interactions on daemon start with redirect message ([7e99589](https://github.com/Kehl-io/nestweaver/commit/7e995892247c12b48afd11bd230804d6d35401ce))
* **cli:** add daemon routing to 10 high-frequency CLI commands ([edc58e5](https://github.com/Kehl-io/nestweaver/commit/edc58e55065f2bbc888aea5551bb04fcb3e806e9))
* **daemon:** extend try_daemon_json_rpc with 19 additional RPC method routes ([3c32110](https://github.com/Kehl-io/nestweaver/commit/3c32110c346d15cec1c7ec7af17461ad24844a73))
* **interactions:** lower flush threshold to 5 and add time-based auto-flush ([31a0bd7](https://github.com/Kehl-io/nestweaver/commit/31a0bd7b1c6fd5e2e838958e87ff734c02e4ea67))
* **mcp:** expand interaction tracking to cover more tools ([908ea8e](https://github.com/Kehl-io/nestweaver/commit/908ea8e4e9cfd505c7f7349f147395b2cdd5ac54))
* **project:** wire tags, parent, and features from ProjectConfig ([c3e059c](https://github.com/Kehl-io/nestweaver/commit/c3e059c584d8616f29f0b556df290041a69bbf80))
* **proto:** extend BrainContextRequest for daemon parity ([6578e58](https://github.com/Kehl-io/nestweaver/commit/6578e585c51ac020a67aab5850c840db00aed178))
* **schema:** add ALL_SYMBOL_EDGE_TYPES constant and from_rel_table_name lookup ([57d0cc3](https://github.com/Kehl-io/nestweaver/commit/57d0cc332d90cc8ddf97df38b25bae59ff02cf12))


### Bug Fixes

* **cli:** rename UnlinkedVault to DiscardedVault, clarify merge output ([970bf13](https://github.com/Kehl-io/nestweaver/commit/970bf1316d46fd08aa209ff99cb0989abb7e577b))
* **store:** preserve SECTION_TAGGED_WITH edges during vault reparent ([859b245](https://github.com/Kehl-io/nestweaver/commit/859b24524c678611828fcd1365b906108f2e5008))
* **store:** prevent data loss in merge_instance_ids via reparent_vault ([0fb6531](https://github.com/Kehl-io/nestweaver/commit/0fb65314261edfe9e379c28af9b08e0afe5e8993))
* **v0.22.0:** instance merge dataloss, interaction tracking, daemon parity, edge-type centralization ([6738965](https://github.com/Kehl-io/nestweaver/commit/67389652e22acb25e4f3d2fdd665f5e7bcbb0e97))

## [0.22.0](https://github.com/Kehl-io/nestweaver/compare/v0.21.0...v0.22.0) (2026-06-13)


### Features

* **brain:** wave-3 — diagnostics polish + brain context filters ([c2caeb6](https://github.com/Kehl-io/nestweaver/commit/c2caeb6137f92dff05c905f05131effd4c3cb9de))
* **instance-remove:** add --purge-graph to cascade-delete instance data ([269635f](https://github.com/Kehl-io/nestweaver/commit/269635f8b535eca630deb1e013a766aafc3bbea3))
* **proto:** forward since/recency_weight through ProjectContextRequest ([979ef1e](https://github.com/Kehl-io/nestweaver/commit/979ef1eca5ebac329958a9592875b255c5196d25))
* **seed-resolution:** graduated path-deboost + kind-priority for symbol lookup ([bbfcb22](https://github.com/Kehl-io/nestweaver/commit/bbfcb226e871409ab434f6efc1e3319f267ee36f))


### Bug Fixes

* **brain-remove:** match stored vault paths regardless of canonical form ([f813959](https://github.com/Kehl-io/nestweaver/commit/f8139590cf6570f58f327d4c89fa4e4daaf9d24e))
* **brain-status:** expose instance_id per row + forward collision warnings through daemon ([0a94cbd](https://github.com/Kehl-io/nestweaver/commit/0a94cbd3b9463e524f3d8cfb4d1c40e98caab293))
* **brain:** close four v0.21.0 regressions surfaced by live-DB audit ([1629156](https://github.com/Kehl-io/nestweaver/commit/16291562ec9f9a0cfb4450e909f7969e01ed435d))
* **engine,cli:** three minor gaps from v0.21.0 adoption validation ([85f44e1](https://github.com/Kehl-io/nestweaver/commit/85f44e124a3e8906b0df065d436129bc8af920bb))
* **engine:** RepoConfig.name resolves repo aliases for projects and features ([dee57d9](https://github.com/Kehl-io/nestweaver/commit/dee57d9cfc9f2834340651d5309da26a8eeb71fc))
* **mcp:** route brain_status through JSON pass-through RPC ([889fa06](https://github.com/Kehl-io/nestweaver/commit/889fa06a17ecc432ee5b8b5b5739328d3d771bb2))
* **project-context:** surface member symbols, not just notes ([cd753e7](https://github.com/Kehl-io/nestweaver/commit/cd753e7bf25112cfabf59571386ecb8b1ebf6188))
* **purge-instance:** sweep orphan symbol/file/note rows by UID prefix ([c8c9cdb](https://github.com/Kehl-io/nestweaver/commit/c8c9cdb31ee6c96557bdb92eea3ccfd1c93a0e09))
* **v0.21.0:** regression fixes, adoption validation, and performance ([326845e](https://github.com/Kehl-io/nestweaver/commit/326845e16e3b08873402dcb4cbb559a7040ef413))


### Performance Improvements

* **cross-domain:** batch edge commits in single transactions ([1597555](https://github.com/Kehl-io/nestweaver/commit/15975552fcdb1e631233b324c960c2686c938b63))
* **engine,cli,mcp:** dedup Heading/Section pairs + daemon path for project-context ([bf95636](https://github.com/Kehl-io/nestweaver/commit/bf956360fce2232f0387bf102671aaa9a7821641))
* **store:** single-query orphan sweep and allocation-free kind_rank ([c35ec60](https://github.com/Kehl-io/nestweaver/commit/c35ec6052358346bd6004f3c9f337ef971d589ce))

## [0.21.0](https://github.com/Kehl-io/nestweaver/compare/v0.20.0...v0.21.0) (2026-06-10)


### Features

* **cli:** add --token-budget to nestweaver context ([91d84b6](https://github.com/Kehl-io/nestweaver/commit/91d84b61aa4099533ba7c116a062d6603996a8eb))
* improve graph UI readiness ([384a0c2](https://github.com/Kehl-io/nestweaver/commit/384a0c2316faf4e362b9106af54377822b96566a))
* **store:** config-driven test path deboost patterns ([7bdb2eb](https://github.com/Kehl-io/nestweaver/commit/7bdb2eb1543f8703b301822463c5ef6c5512c002))
* **web:** add contextual graph actions ([90c5e8d](https://github.com/Kehl-io/nestweaver/commit/90c5e8df54cb675d569188f676ed825425f37ea9))
* **web:** add dense graph views ([46ec6f9](https://github.com/Kehl-io/nestweaver/commit/46ec6f96408eaed56be6fe3969d9de6723a1f6b5))
* **web:** add overview API ([9d0814f](https://github.com/Kehl-io/nestweaver/commit/9d0814fc181f1f3d24d972b3d292976af1ed154e))
* **web:** add overview frontend types ([450d1ab](https://github.com/Kehl-io/nestweaver/commit/450d1aba3e7f12cb105cffee6c857c57aeff0999))
* **web:** add overview guidance surfaces ([06ac75b](https://github.com/Kehl-io/nestweaver/commit/06ac75bba8fb3c047d407eae9d874c5846a65dd6))
* **web:** bring graph nodes to life ([88f6e7f](https://github.com/Kehl-io/nestweaver/commit/88f6e7ffa78f2fe46effd54a16765c378fdbb729))
* **web:** group graph controls ([71b6fa2](https://github.com/Kehl-io/nestweaver/commit/71b6fa286a4bf8b2c3fae55f6d9ab33a70e3864b))
* **web:** load overview graph by default ([b9cbec1](https://github.com/Kehl-io/nestweaver/commit/b9cbec19bf1a85b7bf5e36c0a75acd36784505e9))


### Bug Fixes

* **brain:** remove ghost vault rows by path match, not just computed UID ([b5c162b](https://github.com/Kehl-io/nestweaver/commit/b5c162b9d441d7fbd27af97e4d3b15323b643c80))
* **cli:** resolve clippy warnings in daemon routing guards ([7c393e7](https://github.com/Kehl-io/nestweaver/commit/7c393e7324cae30fc694a7cf5f567e1eec2a6f44))
* **cli:** route brain context and 5 docgraph commands through daemon ([a56137f](https://github.com/Kehl-io/nestweaver/commit/a56137f476344c1b8a54ece5c250444ac58ea160))
* **cli:** route impact, regex-search, hubs, stale-check, status through daemon ([5378550](https://github.com/Kehl-io/nestweaver/commit/5378550b37cbec202174715ec1bbe1636e0e59c8))
* **daemon:** route `brain reindex-search` through daemon RPC ([ce70e67](https://github.com/Kehl-io/nestweaver/commit/ce70e678c13e475493743631ce182d669b577d16))
* **proto:** add instance_id to IndexVaultRequest, thread through daemon and MCP ([5b6855e](https://github.com/Kehl-io/nestweaver/commit/5b6855ed781d0ab6002b70fa8df68c06f20ffa02))
* resolve 6 reported bugs — lock warning, intent, interactions, merge, seed ranking, limit ([cdb97b2](https://github.com/Kehl-io/nestweaver/commit/cdb97b21f340d0ae2c8a73dd83bcfef17eaedf27))
* restore lock-error semantics in open_store, cascade-delete vault notes during merge ([2335fc2](https://github.com/Kehl-io/nestweaver/commit/2335fc25a9aded6c1d53eda435112cd77b5b05db))
* **store:** prefer populated vault on collision merge, warn about unlinked notes ([5d40d2d](https://github.com/Kehl-io/nestweaver/commit/5d40d2df592dcdf478aec5f2b98f98d7f3ed9351))
* **store:** safe vault UID rewrite during instance merge, add --limit docs ([5efa26b](https://github.com/Kehl-io/nestweaver/commit/5efa26b68eb512190e6b7adbcd5d798cb3a3b128))
* **web:** address review findings — brand colors, a11y, dedup, dead code ([025d003](https://github.com/Kehl-io/nestweaver/commit/025d003d0610f5e174247225ed835838e181f780))
* **web:** allow search dropdown to escape header bounds ([24e40a6](https://github.com/Kehl-io/nestweaver/commit/24e40a6004125e9d184031f2f9142f9065743bff))
* **web:** balance overview landmarks ([fbea9d6](https://github.com/Kehl-io/nestweaver/commit/fbea9d6760d14371e6c20585c2916f9d66960c2f))
* **web:** gate overview actions by supported targets ([f6e9aa8](https://github.com/Kehl-io/nestweaver/commit/f6e9aa8ac761e34e93983f0761f41104d630f48e))
* **web:** guard overview graph updates ([3ead808](https://github.com/Kehl-io/nestweaver/commit/3ead808dd4bcb81a71d01f570d1eea7f31ee4ece))
* **web:** make overview entry points actionable ([80694f6](https://github.com/Kehl-io/nestweaver/commit/80694f6b08ad80308e4c3373273d90eb3cd716f5))
* **web:** migrate persisted graph mode ([03782ac](https://github.com/Kehl-io/nestweaver/commit/03782ac38bdf5ea3abd5aaa9cabb509ec365a323))
* **web:** polish guided overview verification ([88887cf](https://github.com/Kehl-io/nestweaver/commit/88887cffb19ffa9900dddf910426888026a1f543))
* **web:** preserve overview landmark labels ([641a732](https://github.com/Kehl-io/nestweaver/commit/641a73237dd8e36aa20c74508155920baed267ef))
* **web:** preserve repo git suffix in overview ([3e2cd7f](https://github.com/Kehl-io/nestweaver/commit/3e2cd7fe4c16e942bc0b7529a653a1f9aaf23447))
* **web:** refine guided overview browser pass ([35f3e6a](https://github.com/Kehl-io/nestweaver/commit/35f3e6ab250a653edea4fd847e6b27934ef3bd73))
* **web:** reset graph mode on page load ([8ead92b](https://github.com/Kehl-io/nestweaver/commit/8ead92b158f2a95282f6f33fd288016a548e1fad))
* **web:** scope e2e search click to dropdown, not graph label ([9ab17e4](https://github.com/Kehl-io/nestweaver/commit/9ab17e48e2cd5c9a814bdaad60ba763c911e6de5))
* **web:** sharpen graph nodes and labels ([106a6ef](https://github.com/Kehl-io/nestweaver/commit/106a6ef18e04828b7917100ed580c99402db1540))
* **web:** simplify graph nodes to led dots ([56bd04e](https://github.com/Kehl-io/nestweaver/commit/56bd04e7fba864d358c5f0cf212821d14bf40fc5))
* **web:** soften graph node borders ([6ab3b00](https://github.com/Kehl-io/nestweaver/commit/6ab3b00e7848ba598e862ea8f4239aa1422d1d19))

## [0.20.0](https://github.com/Kehl-io/nestweaver/compare/v0.19.0...v0.20.0) (2026-06-08)


### Features

* **brain:** support cross-vault wikilinks with vault:note prefix ([5e28d75](https://github.com/Kehl-io/nestweaver/commit/5e28d758dcf820ea6b5c772ac6daf3e22e7c0946))
* **guide:** add CLAUDE.md generation format ([3dde889](https://github.com/Kehl-io/nestweaver/commit/3dde889b154a5c0570f157c941373e4304f4e697))
* markdown knowledge graph enhancements — CLAUDE.md gen, AgentConfig, canvas/dataview/mermaid parsers ([857cbbb](https://github.com/Kehl-io/nestweaver/commit/857cbbb1fa4147b019331a73d22b5d391fdc2d1f))
* **parser:** add Dataview DQL query parser ([5408a9f](https://github.com/Kehl-io/nestweaver/commit/5408a9ff28cfdfbdf0326162256e2f7075c5b126))
* **parser:** add Mermaid flowchart/graph diagram parser ([4aef510](https://github.com/Kehl-io/nestweaver/commit/4aef51056c4a2ef5a44765bf77fc53b25d544274))
* **parser:** add Obsidian canvas file parser ([e2918c9](https://github.com/Kehl-io/nestweaver/commit/e2918c91b86e068c2c6afe6f00b528172884eae7))
* **parser:** extract checkboxes and ADR sections from markdown ([2ff7e0b](https://github.com/Kehl-io/nestweaver/commit/2ff7e0bbb6375f71544ef587684934008dac53af))
* **schema:** add NoteKind::AgentConfig for agentic instruction files ([06e2780](https://github.com/Kehl-io/nestweaver/commit/06e27805cf1690e2170bcf446f1db07acc75291e))
* **setup:** add PreToolUse hook to intercept grep/find, fix token savings claims to validated 10x ([e3bd6dc](https://github.com/Kehl-io/nestweaver/commit/e3bd6dc1a45ebdf4fabb52e1f7a0d5057950b05d))
* **setup:** install Claude Code hooks, add token-savings messaging to skills and cursor rules ([fdd4867](https://github.com/Kehl-io/nestweaver/commit/fdd48673e2f89142de989f9b555ebf1e0e294619))


### Bug Fixes

* address code review findings — AgentConfig round-trip, drive letter guard, leaked configs, dead code, dataview tag regex ([dc27560](https://github.com/Kehl-io/nestweaver/commit/dc275608f007157c2e261d2d1680afd6a6c5f2bb))
* **cli:** stop daemon before direct-mode index to prevent lock contention ([3f65f63](https://github.com/Kehl-io/nestweaver/commit/3f65f63e811e089e1cd5c0a6d075e4983bd1734d))
* collapse nested if-let per clippy ([6760593](https://github.com/Kehl-io/nestweaver/commit/67605938fc57150ff81168e1d7ddb9d137861f11))
* **mcp:** log cache serialization failures instead of silently skipping ([acf771f](https://github.com/Kehl-io/nestweaver/commit/acf771f1712e0806399e535ec3d7cef040b437ad))
* rustfmt formatting for new parser modules ([19b4177](https://github.com/Kehl-io/nestweaver/commit/19b4177dd85bedde21c548bf2f2a6c26ee0eb1ba))
* **store:** address review findings — transaction safety, ordering, cache version ([9fb2ce4](https://github.com/Kehl-io/nestweaver/commit/9fb2ce4f30baebbd60ff7962dd0766b726801d55))
* **store:** hash vec lengths in scope_hash to prevent prefix collisions ([242390b](https://github.com/Kehl-io/nestweaver/commit/242390bd42b1cb3ae6fa628fe1029efcdb2cd60f))


### Performance Improvements

* comprehensive performance audit — 35x index, 10000x cache, parallel resolution ([efbff3b](https://github.com/Kehl-io/nestweaver/commit/efbff3b938f9bb21ee8feddbdeef080493b19a65))
* **engine:** parallelize type environment construction with rayon ([b92a7c4](https://github.com/Kehl-io/nestweaver/commit/b92a7c48dbb693439ea3c1f5f9e3f8a364375f1e))
* **engine:** retain parsed source to avoid redundant disk re-reads ([0028f87](https://github.com/Kehl-io/nestweaver/commit/0028f87fc8d8f5281b567e707aa105231aee56aa))
* **engine:** use bulk delete for force re-index cleanup ([42be7a5](https://github.com/Kehl-io/nestweaver/commit/42be7a53d803b43a3c529a0a6d686f3ed553d060))
* **mcp:** hold response cache in-process with periodic flush ([bebc4d3](https://github.com/Kehl-io/nestweaver/commit/bebc4d3aad6af6c2ff2e663bd63ffa2bda104878))
* **parser:** cache compiled tree-sitter Query objects globally ([31dc3f5](https://github.com/Kehl-io/nestweaver/commit/31dc3f5082eb1d562bcb3f0fa1cb46245a9ff611))
* **resolver:** parallelize per-file reference resolution with rayon ([d67f3b4](https://github.com/Kehl-io/nestweaver/commit/d67f3b4e3f8437a9c630cc6b39e5582078919b74))
* **resolver:** use binary search for find_enclosing_symbol (O(n) → O(log n)) ([2423699](https://github.com/Kehl-io/nestweaver/commit/2423699a8c27f7914a7ecf8896600584db0d2278))
* **store:** add _on variants for markdown batch inserts ([d77e747](https://github.com/Kehl-io/nestweaver/commit/d77e747ba91dcf215a1309ad0d91c0f5b8eceaa1))
* **store:** batch symbol lookups to eliminate N+1 after PPR ([4220cad](https://github.com/Kehl-io/nestweaver/commit/4220cad448e2b75f1b22cb040d77ea1ff6a47198))
* **store:** batch Tantivy commits and use bulk section/heading loaders ([43468ed](https://github.com/Kehl-io/nestweaver/commit/43468ed0453127c2f9d6b68c712af451735edefc))
* **store:** cache PPR adjacency graph keyed on (generation, scope, intent) ([ffd3fba](https://github.com/Kehl-io/nestweaver/commit/ffd3fba5f631c61b66cea8d0203ecc38a929b694))
* **store:** cache symbol name index keyed on graph generation ([c577725](https://github.com/Kehl-io/nestweaver/commit/c5777259d56cc401150c101ff7598310c6dea6ee))
* **store:** replace per-row cascade deletes with bulk DETACH DELETE ([e6df168](https://github.com/Kehl-io/nestweaver/commit/e6df1687ff69f8ff18503560fb8a4367f74acc87))
* **store:** switch response cache to binary format (MessagePack + ZSTD) ([7fed80d](https://github.com/Kehl-io/nestweaver/commit/7fed80d507d175516d1bc46be578f666b6b96c17))
* **store:** wrap vault indexing in single transaction via bulk_vault_write ([9123576](https://github.com/Kehl-io/nestweaver/commit/9123576431dcefaf16834cb6326d5210915b5178))

## [0.19.0](https://github.com/Kehl-io/nestweaver/compare/v0.18.0...v0.19.0) (2026-06-05)


### Features

* **cli:** instance_id from config, instance merge, and duplicate vault warning ([160d40e](https://github.com/Kehl-io/nestweaver/commit/160d40e65a9149ffaddf28dd4ecc960dda59f142))


### Bug Fixes

* **ci:** eliminate zstd link errors and skip daemon tests on Linux ([c017d0f](https://github.com/Kehl-io/nestweaver/commit/c017d0f8b0dcfe13b47ce63de679ab7157958c67))
* **cli:** add stale-check subcommand, body_complete indicator, and snapshot output default ([ffce373](https://github.com/Kehl-io/nestweaver/commit/ffce3734b99f6db230339f4331acc9d4fdcf247c))
* **store:** deduplicate vaults during instance merge ([16bf188](https://github.com/Kehl-io/nestweaver/commit/16bf18803f463c9cb05b7f24bc283979bf35d01e))
* **store:** make insert_vault idempotent and remove obsolete binary ([494caae](https://github.com/Kehl-io/nestweaver/commit/494caaef0bbf2fcb6534e36c52d1f8d345b082b2))
* **store:** use DETACH DELETE pattern in merge_instance_ids ([dbd8bff](https://github.com/Kehl-io/nestweaver/commit/dbd8bff178acd5890e9b343d933b1af85a8413b4))
* **store:** use upsert_vault with DETACH DELETE pattern ([b9bd9c0](https://github.com/Kehl-io/nestweaver/commit/b9bd9c01afbfd225d096864ace99291f4b0f8f4e))
* vault idempotency, stale-check CLI, instance merge, and CI linker fix ([959d6ac](https://github.com/Kehl-io/nestweaver/commit/959d6aca05531b3aa0fa94e53b32e4dff28d72d6))

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
