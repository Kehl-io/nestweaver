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

NestWeaver also writes a codebase guide into `AGENTS.md`, between
`<!-- nestweaver:begin -->` and `<!-- nestweaver:end -->` markers. It creates
the file when it is missing and replaces only the marked span when it exists.
An existing `AGENTS.md` without one pair of markers is left unchanged and the
Codex step reports an error (`setup --force` does not overwrite it); add the
two marker lines where the guide should go and re-run setup.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
