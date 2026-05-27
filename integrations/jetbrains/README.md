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
