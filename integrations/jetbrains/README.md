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

All 38 NestWeaver MCP tools are available, including type-aware context, confidence-weighted impact analysis, investigation bundles, and vault/notes integration.
