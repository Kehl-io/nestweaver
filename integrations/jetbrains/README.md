# NestWeaver JetBrains Integration

## Setup

Run the auto-setup command:

```bash
nestweaver setup jetbrains
```

Or manually create `.junie/mcp/mcp.json`:

```json
{
  "mcpServers": {
    "nestweaver": {
      "command": "nestweaver",
      "args": ["mcp", "--db", "./nestweaver.lbug"]
    }
  }
}
```

All **42** NestWeaver MCP tools are available, including type-aware context, confidence-weighted impact analysis, investigation bundles, and vault/notes integration.

The count is derivable, not typed: the registry is
`all_tool_schemas_undecorated()` in `crates/nestweaver-mcp/src/tools.rs`. Call
`tools/list` to read the live set rather than trusting this line.
