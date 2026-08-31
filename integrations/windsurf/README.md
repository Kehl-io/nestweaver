# NestWeaver Windsurf Integration

## Setup

Run the auto-setup command:

```bash
nestweaver setup windsurf
```

Or manually add to `~/.codeium/windsurf/mcp_config.json`:

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

Restart Windsurf to detect the MCP server.

All **42** NestWeaver MCP tools are available in Windsurf's AI features, including type-aware context, confidence-weighted impact analysis, investigation bundles, and vault/notes integration.

The count is derivable, not typed: the registry is
`all_tool_schemas_undecorated()` in `crates/nestweaver-mcp/src/tools.rs`. Call
`tools/list` to read the live set rather than trusting this line.
