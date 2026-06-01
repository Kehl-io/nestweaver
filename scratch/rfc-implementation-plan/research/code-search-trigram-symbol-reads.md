# Research Foundation: Trigram-Accelerated Regex Search, count_patterns, Symbol-Window Reads, Tiered Display

Research for NestWeaver RFC Features 3, 4, 5, 8. Solo-dev / no-GPU / no-daemon / embedded graph DB constraints.

All sources retrieved 2026-05-29. Vendor/blog claims are explicitly labeled. Nothing here is fabricated; gaps are marked `[UNVERIFIED]`.

---

## Codebase grounding (verified against the tree)

Before the research, two schema facts that change the design:

- **`Symbol` stores `start_line` only — there is NO `end_line`.**
  `crates/nestweaver-schema/src/nodes.rs:140-157`. Fields: `uid, name, kind, repo_uid, file_path, start_line, signature, summary, content_hash, embedding, pagerank_score, is_entry_point, entry_point_kind, visibility, type_info, framework_hint`. The task brief's premise that NestWeaver "stores per-symbol start/end lines" is **only half true** — the end of a symbol's span is NOT currently persisted. Feature 5 must either (a) add `end_line` to `Symbol` (schema-version bump) or (b) derive the span at read time (next-symbol-start heuristic / re-parse). This is the single biggest hidden cost in Feature 5.
- **`Section` stores `start_line`, `end_line`, AND full `text`** (`nodes.rs:241-254`); `Heading` stores `start_line`+`end_line` (`nodes.rs:226-235`). So for the *notes* domain, symbol-window reads and inline-body display are essentially free — the spans and text already exist.
- Tantivy already indexes notes/sections (`crates/nestweaver-store/src/tantivy_index.rs`); `regex` crate is already a dependency (in `nestweaver-parser`, `Cargo.toml:30`). No new heavy dependency is needed for Features 3/4 beyond `regex-syntax` (a sub-crate of `regex`, already transitively present).

---

# FEATURE 3 — Trigram-accelerated regex search

## 3.1 Research foundation / primary sources

- **Russ Cox, "Regular Expression Matching with a Trigram Index (or How Google Code Search Worked)", Jan 2012.** https://swtch.com/~rsc/regexp/regexp4.html (retrieved 2026-05-29). This is *the* canonical reference and the design should follow it directly.
  - **Indexing:** extract every overlapping 3-character substring. `"Google"` → `Goo, oog, ogl, gle`. Each trigram maps to a posting list of document IDs that contain it (an inverted index). Google's `codesearch` stores **only document IDs, not positions** — very space-efficient, but performance degrades as corpus grows because it cannot verify trigram adjacency (per the codesearch index docs, below).
  - **Regex → trigram query (the part that matters):** the algorithm walks the regex computing five properties per sub-expression: `emptyable`, `exact` (the finite set of exact match strings, or "unknown"), `prefix` set, `suffix` set, and `match` (a boolean trigram query — an AND/OR tree of trigram-membership predicates). Concatenation cross-products prefix/suffix sets; alternation (`|`) becomes OR; repetition (`*`,`+`,`?`) widens prefix/suffix. `/Google.*Search/` compiles to `Goo AND oog AND ogl AND gle AND Sea AND ear AND arc AND rch`.
  - **Information-saving vs information-discarding transforms:** when prefix/suffix/exact sets get too large, the algorithm *flushes* their trigram requirements into the `match` AND/OR tree, then truncates the set (or marks `exact = unknown`). This is the memory-bound that keeps query planning from exploding on adversarial regexes.
  - **Two-stage:** trigram query yields *candidate* documents (with false positives); the real `regex` engine then runs only on candidates and removes false positives.
  - **Numbers (primary):** Linux 3.1.3 kernel = **420 MB source → 77 MB index (~18% of source)**. Case-sensitive `hello world`: **36,972 files filtered down to 25 candidates (~100x speedup)**.
  - **Stated limitations:** (1) case-insensitive search produces all-case-variant trigrams per position → much less selective; (2) **a regex with no extractable literals defaults to the `ANY` query, which matches every document — the index gives zero benefit and you fall back to full scan.**

- **`google/codesearch` (Russ Cox, Go, public 2015).** https://github.com/google/codesearch — reference implementation of the above. `index.PostingList(trigram uint32) []uint32` returns the doc-ID posting list per trigram (`https://pkg.go.dev/github.com/google/codesearch/index`). Confirms: posting lists store document IDs (uint32), not positions.

- **`aaw/regrams`** — https://github.com/aaw/regrams — standalone "regex → trigram boolean query" library "in the spirit of codesearch." Useful as an algorithm reference for the AST→query compiler if you don't want to port Cox's `Query` type directly.

## 3.2 Prior art / projects + tradeoffs

| Project | Approach | Tradeoffs / numbers |
|---|---|---|
| **Google codesearch** (Cox) | Doc-ID-only trigram postings + DoS-resistant regex engine | Tiny index (~18% of source); "performance degrades rapidly with large corpus" because no positional verification. Best fit for NestWeaver scale. https://github.com/google/codesearch |
| **Sourcegraph Zoekt** (orig. Han-Wen Nienhuys @ Google) | **Positional** trigrams (store byte/rune offset of each occurrence) | RAM = **1.2× corpus**; index ≈ **3× corpus** (2× offsets + 1× content); shard ≈ **3.5× corpus**. Target **sub-50ms on ~2 GB (Android)**. UTF-8 handled via rune-offset table every 100 runes (ASCII bypasses). ctags symbols ranked higher. https://github.com/sourcegraph/zoekt/blob/main/doc/design.md (retrieved 2026-05-29) |
| **livegrep** (Nelson Elhage, 2015) | **Suffix array** instead of trigrams | No false positives (exact substring positions); regex compiled to an `IndexKey` (edges with min/max byte ranges) walked via chained binary searches, then RE2 verifies. Suffix arrays are larger and harder to update incrementally → poor fit for live re-indexing. https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/ |
| **Hound** (Etsy, now hound-search) | Trigram, "based directly on Cox's article" | Simple, single-server, fast; no semantic/code-intelligence. Good proof that a minimal Cox-style trigram index is production-viable. https://github.com/hound-search/hound |
| **GitHub Blackbird** (2023, Rust) | **Sparse grams**, not fixed trigrams | "Trigrams aren't selective enough" for tokens like `for` at GitHub scale → false-positive blowup. Sparse-gram tokenization picks intervals where inner bigram weights < border weights. Index incl. content ≈ **¼ of 115 TB corpus** (28 TB unique → 25 TB index). P99 shard ~100 ms. **Sparse grams are overkill for solo-dev scale** but document the selectivity failure mode of plain trigrams. https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/ |
| **Cursor "Fast regex search"** (Vicent Marti, 2026-03-23) | Sparse n-gram, **client-local, mmap'd** | Directly motivated by *agent* use: local index avoids network latency and stays "very fresh" so an agent reading its own writes finds them; "if the agent is searching for specific text and it does not find it, it'll often go into a wild goose chase, waste tokens." Postings file (sequential) + sorted hash lookup table (mmap, binary search). Literal extraction: trigrams from literal runs; alternations → OR; small char classes expanded, broad classes skip cross-boundary trigrams. No explicit timeout/budget discussed. https://cursor.com/blog/fast-regex-search (vendor blog) |

## 3.3 Recommended approach for NestWeaver

**Adopt Cox/codesearch (doc-ID-only postings), NOT Zoekt-positional.** Rationale: NestWeaver is solo-dev, no daemon, embedded DB; corpus is one-to-few repos + a vault, not Android/Chrome. Positional index buys 1.2× RAM and complexity NestWeaver doesn't need at this scale. Plain trigrams' selectivity problem (Blackbird) only bites at GitHub scale.

- **Index unit = document = one of {Symbol body span, Section, Note}.** Reuse the existing UID as the doc ID (or a dense u32 mapping). For Symbols, the "body" is the source span — see Feature 5 caveat about missing `end_line`; until that lands, index the signature + summary, or the line range derived at index time.
- **Index schema / sidecar:** new sidecar `<db>.trigrams.json` (or a compact binary `<db>.trigrams.idx`) consistent with the existing sidecar convention (`<db>.pagerank.json`, `<db>.tantivy/`, etc.). Map: `trigram (u32, 3 bytes packed) → sorted Vec<u32> doc-ids`. Delta-encode + varint the posting lists to keep size near Cox's ~18%. Build during `index`; incrementally update on `watch` (re-tokenize only changed docs via the existing `<db>.filemeta.json` change detection — this is why doc-ID postings beat suffix arrays for NestWeaver's live re-indexing).
- **Query planning:** use **`regex-syntax::hir::literal::Extractor`** (already in-tree transitively via `regex`) rather than hand-rolling Cox's analyzer. Parse pattern → HIR → `Extractor::extract` (Prefix kind) → `Seq`. For each literal in the Seq, emit `AND` of its trigrams; across alternatives in the Seq, `OR`. Then intersect/union posting lists (galloping intersection on sorted Vec<u32>). Hand the candidate doc set to the real `regex::Regex` for verification. (API detail in §3.5.)
- **Fallback (required):** if `Seq` is **infinite** (`.is_finite() == false`, e.g. `[A-Z]+`, `.*`, leading wildcard) or all literals are shorter than 3 bytes after dropping `inexact` ones → **no usable trigrams → full scan** over all docs (or scoped by repo/kind filter). This mirrors Cox's `ANY` fallback. Always correct, just not accelerated.
- **Budget/timeout:** (a) cap candidate-set size — if the trigram query yields > N candidates (e.g. > 30% of corpus), the prefilter isn't earning its keep; either run anyway or bail to scan. (b) Wall-clock timeout on the verification pass (`regex` is linear-time/DoS-safe, but large corpora still cost) — return partial results + a `truncated: true` flag. (c) Use `Extractor::limit_total` / `limit_class` / `limit_repeat` to bound planning cost on adversarial patterns.

## 3.4 Pitfalls / failure modes + mitigations

- **No extractable literals** (`.*`, `\w+`, `[A-Z]{3}`): index useless. *Mitigation:* detect infinite `Seq`, fall back to scan; document this so users know `foo.*bar` is fast but `\w+` is not.
- **Index bloat:** plain trigram index ~18% of source (Cox) is fine; but storing posting lists naively (Vec<u32> JSON) bloats. *Mitigation:* delta+varint encode; binary sidecar, not JSON, for the postings.
- **Selectivity collapse on common trigrams** (`for`, `int`, ` th`): huge posting lists, AND-intersection still scans a lot. *Mitigation:* prefer the *rarest* trigrams in the AND (intersect shortest postings first / skip the most common trigrams from the query) — Blackbird's sparse-gram is the heavy-duty version; for NestWeaver, simply order intersection by posting-list length.
- **Unicode / multi-byte:** trigrams over raw UTF-8 bytes mostly work but a 3-byte CJK char = one trigram window oddity. *Mitigation:* tokenize over bytes (Cox does); ASCII-fast-path; accept minor selectivity loss on non-ASCII. Don't build the rune-offset table (Zoekt) — not worth it at this scale.
- **Case-insensitive** `(?i)`: trigram set explodes to all case variants → low selectivity (Cox's own caveat). *Mitigation:* lowercase-fold both index and query into a parallel case-folded trigram set, OR just fall back to scan for `(?i)` with no strong literal. Don't pretend `(?i)` is as fast as case-sensitive — Cox explicitly says it isn't.
- **Stale index after edits:** agent edits a file, searches, trigram index lags → false "not found" (the exact failure Cursor calls out). *Mitigation:* tie trigram updates to the existing `watch`/`graph_generation` machinery; re-tokenize changed docs synchronously on re-index.

## 3.5 Complexity / effort + numbers

- **Effort: Medium-High.** Tokenizer (trivial), posting-list store + delta/varint codec (small), regex→trigram-query compiler **using `regex-syntax` Extractor** (medium — the Seq→trigram-AND/OR mapping and the infinite/inexact handling are the fiddly part), posting-list intersection (small), verification + budget (small), incremental update on watch (medium).
- **Reported numbers to set expectations:** Cox **~18% index / ~100× filter speedup**; Zoekt **sub-50ms / 1.2× RAM / 3× index** (positional, upper bound you're avoiding); Blackbird **~25% incl. content** at extreme dedup scale. For NestWeaver, target **index ≈ 15-25% of indexed-text size, candidate-set verification in single-digit ms** for typical literal-bearing queries.
- `regex-syntax` literal `Extractor` API (https://docs.rs/regex-syntax/latest/regex_syntax/hir/literal/struct.Extractor.html, retrieved 2026-05-29): `Extractor::new().kind(ExtractKind::Prefix).extract(&hir) -> Seq`. `Seq` is finite / infinite / empty; literals are **exact** (definitive) or **inexact** (candidate, regex must confirm — happens across char classes/reps). Config: `limit_class`, `limit_repeat`, `limit_literal_len`, `limit_total`. Look-arounds treated as matching empty string (`\bquux\b` → literal `quux`). **Key: check `Seq::is_finite()` and per-literal exactness to decide AND/OR planning vs scan fallback.**

---

# FEATURE 4 — `count_patterns` (counts-only companion)

## 4.1 Research foundation / prior art

No distinct primary literature — this is `grep -c` / `rg --count` / `rg --count-matches` semantics layered on the Feature 3 prefilter. Anchors:
- **ripgrep `--count` (file + match count) and `--count-matches` (total occurrences).** https://github.com/BurntSushi/ripgrep — the established UX split between "files matched" vs "total occurrences" is exactly the two numbers to report.
- **Andrew Gallant, "ripgrep is faster than {grep, ag, …}", 2016.** https://burntsushi.net/ripgrep/ (retrieved 2026-05-29). Counting still requires scanning each matching line, but ripgrep stays out of the regex engine using *inner-literal* extraction + Aho-Corasick / Teddy (Geoffrey Langdale, Intel Hyperscan) / `memchr` (SIMD, multi-GB/s) to skip non-candidate lines. Same prefilter philosophy as Feature 3, just don't materialize match spans/snippets.

## 4.2 Recommended approach

- **Reuse the Feature 3 trigram prefilter verbatim** to get candidate docs, then run `regex` in count mode (`find_iter().count()` per doc; track files-with-≥1-match separately). Return `{total_files_matched, total_occurrences, per_file: [{doc_uid/path, count}]}`.
- **Multi-pattern in one call:** accept `Vec<pattern>`. For each pattern build its own trigram query; **union the candidate sets** (a doc that could match *any* pattern), scan each candidate once, attribute counts per pattern. This is cheaper than N independent calls because the candidate union and per-doc text fetch are shared. Report per-pattern and combined totals.
- No snippets, no line numbers → much cheaper than a full search and very token-cheap to return to an agent.

## 4.3 Pitfalls + mitigations

- **Overlapping vs non-overlapping match counts** — `regex` `find_iter` is non-overlapping/leftmost; document this (matches `rg --count-matches`). Don't promise overlapping counts.
- **`(?i)` / no-literal patterns** inherit Feature 3's scan fallback — counts are still correct, just unaccelerated.
- **Per-file list size** can be large on broad patterns → token-budget the `per_file` array (top-N by count + "+M more files"), since the consumer is an agent.

## 4.4 Effort

**Low**, given Feature 3 exists. It is Feature 3's prefilter + verification with span-materialization turned off and a per-doc counter. Main new work: multi-pattern candidate-union and the result shape/budgeting.

---

# FEATURE 5 — Symbol-window file reads (`read_symbols`)

## 5.1 Research foundation / primary sources

- **Aider repo map / tree-sitter tags — Paul Gauthier, "Building a better repository map with tree sitter", 2023-10-22.** https://aider.chat/2023/10/22/repomap.html (retrieved 2026-05-29). Tree-sitter parses to AST; each grammar ships a `tags.scm` query that classifies nodes as *definition* vs *reference*. Aider deliberately shows **signatures / "critical lines," not full bodies**, to conserve context, and ranks via PageRank into a **token budget (`--map-tokens`, default 1,000 tokens)**. This is the canonical "symbol-precise, token-budgeted code context for an LLM" prior art and validates Feature 5's whole premise.
- **tree-sitter** itself: AST nodes carry precise byte/line spans → the correct primitive for "give me exactly this symbol's span." `tags.scm` also defines comment nodes for language-aware comment stripping.
- **ctags / universal-ctags vs tree-sitter vs LSP `textDocument/documentSymbol`:** ctags = regex-based, fast, imprecise (can't truly parse a language); tree-sitter = AST, accurate, 130+ grammars; LSP documentSymbol = semantic but requires a running language server (a daemon — violates NestWeaver's no-daemon constraint). https://github.com/chrismwendt/ctags-vs-tree-sitter (perf comparison). **Conclusion: tree-sitter is the right precision tool; NestWeaver already uses it for parsing, so spans are obtainable.** `npezza93/ttags` shows ctags-from-tree-sitter is a real pattern.

## 5.2 Token-savings evidence — ALL VENDOR/BLOG, NOT PEER-REVIEWED

Mark every one of these as unverified vendor/blog claims, not measured fact:

- **[VENDOR/BLOG]** "A targeted 20-line read is 10-50× cheaper than reading the full file… ~18,000 tokens vs ~800 tokens." / "the body of a specific function — 400 tokens instead of 6,000." — https://fazm.ai/t/reduce-ai-agent-token-costs-mcp-strategies (retrieved 2026-05-29). Marketing for a token-cost-reduction product.
- **[VENDOR/BLOG]** "save 94% of tokens by indexing once… real-world broader questions 40-55% savings… pure comprehension 70-95%." — https://elara-labs.github.io/code-context-engine/ (retrieved 2026-05-29). Vendor landing page.
- **[BLOG]** "70% of tokens were waste" (one developer's week-long self-measurement) — https://dev.to/nicolalessi/... (retrieved 2026-05-29). Anecdotal, single-author, no methodology.

**Assessment:** The RFC's "30-60% token reduction" goal is *plausible and consistent with vendor claims*, but there is **no peer-reviewed or independently reproduced measurement** in the retrieved sources. Feature 5 should ship with its own before/after token instrumentation rather than cite these numbers as fact. The directionally-solid, non-vendor support is structural (Aider deliberately omits bodies to fit a 1k-token budget) — that's an existence proof that token-budgeted symbol context works, not a percentage.

## 5.3 Recommended approach for NestWeaver

- **BLOCKER FIRST: `Symbol` has no `end_line`** (`nodes.rs:140-157`). Options, in order of preference:
  1. **Add `end_line: u32` to `Symbol`** during indexing (tree-sitter already gives the node's end byte/row for free in the parser). Requires a schema-version bump (`crates/nestweaver-schema/src/version.rs`) and a re-index. Cleanest; makes `read_symbols` an O(1) span fetch. **Recommended.**
  2. **Derive at read time:** span = `[start_line, next_symbol_in_file.start_line - 1]`. Cheap, no schema change, but wrong for nested symbols (a method inside a class), trailing symbols, and interleaved comments. Acceptable only as an interim.
  3. **Re-parse the file on demand** to recover the node span. Accurate but defeats the token/latency goal.
- **Span read:** given an FQN or name, resolve to Symbol UID (reuse existing symbol resolution; handle ambiguity with the existing exit-code-3 / multiple-match behavior), read `file_path` lines `[start_line, end_line]`. Notes/Sections are already trivial — `Section` carries `start_line`,`end_line`,`text`.
- **Comment stripping (optional, off by default):**
  - *Tree-sitter languages:* query comment nodes (`comment`, `line_comment`, `block_comment` per grammar) and elide them — accurate.
  - *Regex-only languages (NestWeaver parses 32 langs, some via regex):* **conservative line-prefix strip only** (strip lines whose first non-whitespace token is the line-comment marker `//`, `#`, `--`, etc.). Do NOT attempt block-comment or mid-line stripping with regex — see pitfalls.
- **N adjacent symbols:** include ±N siblings by `start_line` order within the same file (cheap once spans exist). Useful for "show me this function and its neighbors."
- **FQN resolution:** map `module::Type::method` / `path/file.rs::fn` to UID via the graph; the brief's `read_symbols` should accept name, FQN, or `file::symbol`.
- **Token-budget aware:** sum estimated tokens across requested spans; if over budget, drop lowest-priority adjacent symbols first, then fall back to signature-only (Aider-style) for overflow, and flag `truncated`.

## 5.4 Pitfalls / failure modes + mitigations

- **Missing `end_line` (the core risk)** — mitigations above; prefer schema add.
- **Comment-strip false elision** — the brief's named risk. Stripping `# ...` blindly corrupts: shebangs (`#!/bin/sh`), `#` inside string literals (`"a # b"`), Python f-strings, CSS `#id`, URLs in strings (`https://`→`//`!), regex langs where `//` is division or part of a path. *Mitigation:* (1) default OFF; (2) tree-sitter comment-node strip is safe — prefer it whenever a grammar exists; (3) for regex langs, only strip a *whole line* when the comment marker is the first non-whitespace char AND the line isn't inside a known string/heredoc context you can cheaply detect — when in doubt, **don't strip** (false elision of code is worse than leaving a comment). Never strip mid-line for regex langs.
- **Off-by-one line indexing** — `start_line` is almost certainly 1-based (tree-sitter rows are 0-based; check the parser's conversion). Verify before slicing files.
- **Stale spans after edits** — if the file changed since index, stored spans drift. *Mitigation:* compare `content_hash` / `filemeta` mtime; if stale, re-derive or warn.
- **Ambiguous names** — multiple symbols named `greet`. *Mitigation:* reuse existing exit-code-3 ambiguity handling; return all or require FQN.

## 5.5 Effort

**Medium.** The blocker is the `end_line` schema decision (+re-index). Span read, FQN resolution, adjacency, budgeting are straightforward and reuse existing graph/resolution code. **Comment stripping is the deceptively expensive part** — language-aware tree-sitter stripping is per-grammar work and the regex-lang heuristics are correctness-sensitive; recommend shipping span-read first, comment-stripping as a clearly-flagged follow-up.

---

# FEATURE 8 — Tiered display (inline body for high-confidence hits)

## 8.1 Research foundation / prior art

- **Cursor "Fast regex search" (2026), the agent-round-trip argument.** https://cursor.com/blog/fast-regex-search — "if the agent is searching for specific text and it does not find it, it'll often go into a wild goose chase, waste tokens." Inlining the body on a confident hit removes the follow-up read round-trip that an agent would otherwise issue — directly the Feature 8 motivation (vendor blog).
- **Aider repo map** (above) — establishes that a tool can return *graded* code context (signature-only vs key-lines) under a token budget rather than all-or-nothing. Feature 8 is the same idea applied to search hits: high-confidence → inline body; lower → just the locator.
- **Zoekt ranking** (https://github.com/sourcegraph/zoekt/blob/main/doc/design.md) — produces a normalized relevance score per hit from match count, proximity, word-boundary alignment, symbol-definition (ctags), filename, recency, optional BM25. Establishes that a defensible normalized score exists to threshold on. NestWeaver already has analogous signals (PageRank, BM25 via Tantivy, symbol-kind).

## 8.2 Recommended approach

- **Threshold inline at normalized relevance ≥ 0.75** (the RFC's number). Inline = the symbol/section body via Feature 5's span read (Sections already have `text`; Symbols need the `end_line` from Feature 5). Below threshold → return locator only (path + line + signature) and let the agent decide to read.
- **Normalization is the load-bearing design choice.** A raw hybrid score (BM25 + PageRank + trigram match count) is unbounded/scale-dependent; 0.75 is meaningless unless scores are mapped to [0,1]. Use either min-max over the result set, or a softmax/percentile normalization, and apply a *gap* check (only inline if the top hit is clearly separated from the rest) to avoid inlining 50 bodies on a flat score distribution.
- **Token budget interaction:** inlining bodies is exactly what blows a context window — cap total inlined tokens (e.g. inline top-K confident hits until budget, then degrade to signature-only, then locator-only), reusing Feature 5's budgeter.

## 8.3 Pitfalls + mitigations

- **Threshold mis-calibration** — 0.75 on an un-normalized score inlines everything or nothing. *Mitigation:* normalize per query + require a confidence *gap*, not just an absolute floor.
- **Token blowup** — inlining many large bodies defeats the token-saving goal it shares with Feature 5. *Mitigation:* hard cap on inlined tokens / max-K inlined hits.
- **Large symbol bodies** — a 600-line god-function at 0.9 confidence shouldn't be inlined whole. *Mitigation:* cap inlined span size; inline signature + first N lines + "…(read_symbols for full body)".
- **Confidence ≠ correctness** — a high score can still be the wrong symbol. Inlining doesn't make it right; it just saves a round-trip when it is. Keep the locator alongside the inlined body so the agent can verify/expand.

## 8.4 Effort

**Low**, layered on Features 3+5. New work: score normalization + gap logic + the inline/locator decision + reusing the token budgeter. No new index or parsing.

---

## Cross-cutting summary

- **Build order:** Feature 5's `end_line` schema add → Feature 3 trigram index (reusing `regex-syntax` Extractor) → Feature 4 (Feature 3 minus snippets) → Feature 5 span reads → Feature 8 (Features 3+5 + normalization).
- **Pick Cox/codesearch doc-ID trigrams, not Zoekt-positional, not livegrep suffix arrays, not Blackbird sparse-grams** — solo-dev scale + live re-indexing favor the smallest/most-incrementally-updatable index. Document the sparse-gram selectivity caveat as a "if we ever hit scale" note.
- **The single biggest landmine** is that `Symbol` has no `end_line` today — every "symbol body" feature (5, and the Symbol path of 3 and 8) depends on adding it.
- **Token-savings numbers are vendor/blog only; ship instrumentation, don't cite percentages as fact.**

## Source list (all retrieved 2026-05-29)

1. Russ Cox, "Regular Expression Matching with a Trigram Index", 2012 — https://swtch.com/~rsc/regexp/regexp4.html
2. google/codesearch — https://github.com/google/codesearch ; index pkg — https://pkg.go.dev/github.com/google/codesearch/index
3. aaw/regrams — https://github.com/aaw/regrams
4. Sourcegraph Zoekt design — https://github.com/sourcegraph/zoekt/blob/main/doc/design.md
5. Nelson Elhage, suffix arrays, 2015 — https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/
6. Hound — https://github.com/hound-search/hound
7. GitHub Blackbird, 2023 — https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/
8. Cursor "Fast regex search", Vicent Marti, 2026-03-23 — https://cursor.com/blog/fast-regex-search (vendor)
9. Andrew Gallant, "ripgrep is faster than…", 2016 — https://burntsushi.net/ripgrep/ ; ripgrep — https://github.com/BurntSushi/ripgrep
10. regex-syntax literal Extractor API — https://docs.rs/regex-syntax/latest/regex_syntax/hir/literal/struct.Extractor.html
11. Aider repo map / tree-sitter, 2023 — https://aider.chat/2023/10/22/repomap.html ; https://aider.chat/docs/repomap.html
12. ctags vs tree-sitter — https://github.com/chrismwendt/ctags-vs-tree-sitter ; ttags — https://github.com/npezza93/ttags
13. [VENDOR] token-savings claims — https://fazm.ai/t/reduce-ai-agent-token-costs-mcp-strategies ; https://elara-labs.github.io/code-context-engine/ ; [BLOG] https://dev.to/nicolalessi/i-tracked-every-token-my-ai-coding-agent-consumed-for-a-week-70-was-waste-465
