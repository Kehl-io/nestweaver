# NestWeaver Codex Integration

## Setup

Run the auto-setup command:

```bash
nestweaver setup codex
```

Or manually add to `.codex/config.toml`:

```toml
[mcp_servers.nestweaver]
command = "nestweaver"
args = ["mcp", "--db", "./nestweaver.lbug"]
```

NestWeaver will also generate an `AGENTS.md` file with codebase intelligence.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
