# NestWeaver Cursor Integration

## Setup

1. Index your repository:
   ```bash
   nestweaver index --repo .
   ```

2. Copy the MCP config to your project:
   ```bash
   cp integrations/cursor/mcp.json .cursor/mcp.json
   ```

3. Restart Cursor to detect the MCP server.

All NestWeaver MCP tools (context, search, impact, flow tracing, clustering, change detection) are available in Cursor's AI chat.
