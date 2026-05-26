---
name: nestweaver-impact
description: Check blast radius before modifying code using NestWeaver.
---

Before modifying a function, class, or module:

1. Call `brain_impact` with the symbol name, depth 3
2. Review the impact nodes — these are the blast radius grouped by depth
3. Call `cross_repo_contracts` if the symbol might be used across services
4. Call `backlinks` if the symbol is referenced in vault notes
5. Report: what will break, what needs updating, and what notes document this decision
