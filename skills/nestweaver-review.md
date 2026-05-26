---
name: nestweaver-review
description: Review code changes with full codebase context from NestWeaver.
---

When reviewing a PR or set of changes:

1. Get the list of changed files from git diff
2. For each changed file, call `brain_context` with the file path as seed
3. Check for vault notes that mention modified symbols
4. Identify cross-repo impacts via `cross_repo_contracts` for any modified public APIs
5. Report: what each change affects, any missing test coverage, and relevant design decisions from notes
