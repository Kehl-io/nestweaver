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

All NestWeaver MCP tools are available in Windsurf's AI features.
