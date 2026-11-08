# Change-impact positioning

Reviewed 2026-11-08 for nw-068. This is a dated comparison of documented
capabilities, not a benchmark, vendor endorsement or claim of exclusive features.

NestWeaver's positioning is a local, queryable code-and-Markdown graph with
confidence-bearing relationships, CLI/MCP retrieval, and explicit freshness and
coverage limits. Its impact and affected-test reports help a reviewer choose
what to inspect and test. They do not establish that a change is safe, prove
dead code, or replace human ownership and review.

Cross-repository blast radius alone is not a unique differentiator:

| Primary source checked | What it supports | Comparison limit |
| --- | --- | --- |
| [GitNexus group CLI](https://github.com/abhigyanpatwari/GitNexus/blob/main/gitnexus/src/cli/group.ts) | A group impact command crosses repository boundaries. | Source documentation; no matched accuracy or latency evaluation. |
| [GitLab Orbit library-change guide](https://about.gitlab.com/resources/well-architected/cookbook/agents/blast-radius-explorer/) | Cross-project caller/import inspection and source review through Duo and Orbit. | The guide describes GitLab.com beta availability and access requirements; no broader availability claim. |
| [Axiom Refract change-impact guide](https://axiomrefract.com/learn/what-is-change-impact-analysis) | File-dependent traversal, dependency chains and test/service impact are described. | Vendor description, not independent validation or proof of symbol-level parity. |
| [Swimm Auto-sync explanation](https://swimm.io/blog/how-does-swimm-s-auto-sync-feature-work) | Keeping code-linked documentation synchronized and requesting human help for uncertain changes. | This is an older, dated explanation of one feature, not an exhaustive current product comparison. |

Use factual capability copy: “Trace callers and dependencies across indexed
repositories, inspect the supporting source, and get scoped tests and warnings
about incomplete or stale evidence.” Avoid “only,” “complete,” or accuracy and
speed comparisons without a frozen, matched evaluation.

NestWeaver has SARIF output and a recall-evaluation workflow. Their presence is
not evidence that competitors lack them or that every reported dependency is
correct. CODEOWNERS reviewer routing remains the separate nw-035 work package;
do not advertise it as delivered. Re-check vendor sources before future
comparative marketing changes. This guidance makes no pricing or cohort claim.
