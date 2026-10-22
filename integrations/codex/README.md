# NestWeaver Codex Integration

## Setup

Run the auto-setup command:

```bash
nestweaver setup codex
```

Or manually add to the project's `.codex/config.toml` (Codex loads a
project-scoped config only for projects you have marked trusted; otherwise use
`~/.codex/config.toml`):

```toml
[mcp_servers.nestweaver]
command = "nestweaver"
args = ["mcp", "--db", "/absolute/path/to/nestweaver.lbug"]
```

`setup` writes the absolute database path, so the server finds the same graph
whatever directory Codex starts it from.

NestWeaver will also generate an `AGENTS.md` file with codebase intelligence.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
