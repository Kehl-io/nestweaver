---
name: nestweaver-impact
description: Check blast radius before modifying code using NestWeaver.
---

Before modifying a function, class, or module:

1. Call `blast_radius` with the changed files for a confidence-weighted risk assessment — each affected symbol has an `impact_score` (0-1) showing how strongly the change propagates through the call graph
2. Call `brain_impact` with the symbol name for a focused symbol-level impact view
3. Call `affected_tests` to identify which tests cover the changed code (test-impact analysis)
4. Use `detect_changes` with the list of files you expect to modify for process-level risk
5. Call `dead_code` to check if any impacted symbols are already unreachable
6. Call `contract_drift` if the symbol is part of a cross-service API contract
7. Call `cross_repo_contracts` if the symbol might be used across services
8. Call `backlinks` if the symbol is referenced in vault notes
9. Report: what will break (sorted by impact_score), what tests to run, what notes document this decision
