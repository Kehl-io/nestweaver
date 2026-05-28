---
name: nestweaver-review
description: Review code changes with full codebase context from NestWeaver.
---

When reviewing a PR or set of changes:

1. Get the list of changed files from git diff
2. Call `detect_changes` with the changed file list for an overall risk assessment
3. For each changed file, call `brain_context` with the file path as seed
4. Use `blast_radius` on modified public symbols to see risk-scored impact
5. Call `dead_code` to check if the changes introduce or interact with unreachable code
6. Check for vault notes that mention modified symbols
7. Identify cross-repo impacts via `cross_repo_contracts` for any modified public APIs
8. Report: what each change affects, any missing test coverage, and relevant design decisions from notes
