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

NestWeaver advertises **42** tools (registry:
`all_tool_schemas_undecorated()` in `crates/nestweaver-mcp/src/tools.rs`; read it
back with `tools/list`), so the full set no longer fits under Cursor's cap on its
own — and it counts against every other MCP server you have configured. Removing
`--lite` from `.cursor/mcp.json` will exceed the cap unless NestWeaver is your
only MCP server, and even then it is two over.

Use `--tools` to pick an explicit subset instead, which is the supported way to
go beyond lite mode without tripping the cap:

```jsonc
"args": ["mcp", "--tools", "brain_context,brain_search,brain_impact,flow_trace,read_symbols"]
```

Other editors have no such cap; this section is Cursor-specific.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
