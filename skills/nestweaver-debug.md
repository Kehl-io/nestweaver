---
name: nestweaver-debug
description: Debug errors using NestWeaver's code+notes graph. Prefer graph queries over grep — brain_context shows the full call chain and related code in one ranked result.
---

**Use NestWeaver to trace errors through the graph.** `brain_context` + `flow_trace` show the call chain and related code without reading files. `brain_search` finds error patterns across both code AND vault notes in one call.

When debugging an error or unexpected behavior:

1. Extract key symbol names from the error message or stack trace
2. Call `brain_search` with the error message keywords
3. For each matched symbol, call `brain_context` to see its call chain and related code
4. Use `investigate` to build a focused investigation bundle around the failing symbol — then `investigate_expand` to widen if needed, `investigate_hydrate` to load full source
5. Use `flow_trace` to trace forward execution from the suspected origin
6. Use `brain_impact` to find all callers of the failing symbol — the `impact_score` shows how strongly each caller depends on it
7. Use `read_symbols` to view the source code of suspects directly
8. Check `dead_code` — if the error is in unreachable code, it may be safe to remove instead of fix
9. Check if any vault notes mention the error pattern via `brain_search`
10. Report: the call chain leading to the error, related code, and any existing documentation

Note: With `--track-interactions` enabled, debugging patterns are remembered across sessions — symbols involved in past debugging flows rank higher in future queries.
