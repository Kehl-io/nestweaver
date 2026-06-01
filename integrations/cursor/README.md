# NestWeaver Cursor Integration

## Quick Setup

```bash
nestweaver index --repo .
nestweaver setup cursor
```

This writes `.cursor/mcp.json` (with `--lite` mode: 6 core tools) and `.cursor/rules/nestweaver.mdc` (agent rules).

## Manual Setup

Copy the MCP config to your project:

```bash
cp integrations/cursor/mcp.json .cursor/mcp.json
```

Restart Cursor to detect the MCP server.

## Lite Mode

Cursor has a 40-tool cap across all MCP servers. NestWeaver uses `--lite` mode by default for Cursor, exposing 6 core tools: `brain_context`, `brain_search`, `brain_impact`, `brain_status`, `brain_guide`, `detect_changes`.

To use all 38 tools, edit `.cursor/mcp.json` and remove `--lite` from the args.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
