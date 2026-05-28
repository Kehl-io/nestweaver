---
name: nestweaver-impact
description: Check blast radius before modifying code using NestWeaver.
---

Before modifying a function, class, or module:

1. Call `brain_impact` with the symbol name, depth 3
2. Call `blast_radius` for a risk-scored assessment (Low/Medium/High)
3. Review the impact nodes -- these are the blast radius grouped by depth
4. Use `detect_changes` with the list of files you expect to modify for overall risk
5. Call `dead_code` to check if any impacted symbols are already unreachable
6. Call `cross_repo_contracts` if the symbol might be used across services
7. Call `backlinks` if the symbol is referenced in vault notes
8. Report: what will break, what needs updating, and what notes document this decision
