# RFC v0.9.1+ implementation plan — research bundle

- **`MASTER-EXECUTION-PLAN.md`** — ⭐ **start here to execute.** One sequenced backlog: read-first
  corrections, status-at-a-glance, dependency-ordered milestones (M0–M5), explicit gates, the
  dependency graph, the fully-scoped first PR (§6), and the independent review findings (§7). The
  build-specs below are the per-item detail; the corrections in §7 override them where they differ.
- **`BUILD-SPEC-phase0-wave1.md`** — the executable front: task-level specs (verified files,
  schema/signatures, ordered steps, TDD tests, acceptance) for Phase 0 enablers + Wave 1
  (F5, F8, F3, F4, F6). Start here to build.
- **`BUILD-SPEC-wave2.md`** — same depth for Wave 2: F9 (five `brain.*` doc-graph tools) +
  F2-core (contract linking scoped to the trustworthy lanes: wire `detect_frameworks` → contract
  nodes → same-repo IMPLEMENTS → safe cross-repo gRPC/operationId → drift).
- **`BUILD-SPEC-wave3.md`** — same depth for Wave 3 (PR-review track, journeys J2/J5): git-history
  mining substrate, F12 (activity multiplier at rank-read, clamp fix), F13 (`affected_tests`,
  mostly reuse of `traverse.rs::impact` + test heuristics).
- **`BUILD-SPEC-wave4.md`** — Wave 4 (eval-gated quality features), scoped to what's *left*: F7's
  PRF pass (alias expansion already shipped) + F1's finish (PPR consumption already wired — add
  `TerminalSuccess`, negative signal, exploration floor, `interactions show --uid`).
- **`BUILD-SPEC-wave5-final.md`** — final wave: F10 (investigate) + F11 (memory-bank) at full depth;
  F14/F15/F16/F17 as spec + carried adversarial gates. **Completes build-ready specs for all 17.**
- **`ADDENDUM-evidence-and-findings.md`** — evidence (the 7+1 journeys, the MCP-config gap),
  verified implementation surface (corrections to the plan), and adversarial findings on F2/F16/F17.
- **`IMPLEMENTATION-PLAN.md`** — the master strategic plan. Read §1 (RFC corrections) and §2
  (cross-cutting foundations) first; they reframe the whole roadmap.
- **`research/`** — seven cited research dossiers (one per feature cluster), each with
  primary sources + URLs, prior-art systems, codebase-grounded recommendations, pitfalls,
  and effort. Produced by parallel research agents required to cite only retrieved sources
  and flag `[UNVERIFIED]` claims.

Bugs #12 and #19 are already implemented on `feat/ui-next-gen-r3f` (working tree).
This bundle covers the 17 remaining features.
