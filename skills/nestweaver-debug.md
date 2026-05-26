---
name: nestweaver-debug
description: Debug errors using NestWeaver's code+notes graph.
---

When debugging an error or unexpected behavior:

1. Extract key symbol names from the error message or stack trace
2. Call `brain_search` with the error message keywords
3. For each matched symbol, call `brain_context` to see its call chain
4. Check if any vault notes mention the error pattern via `brain_search`
5. Report: the call chain leading to the error, related code, and any existing documentation
