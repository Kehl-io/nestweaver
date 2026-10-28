# Connect and verify an AI agent

Install `nestweaver@12.2.4`, index the repositories you need, then configure the
client with the same absolute database path. No model or API key is required for
structural queries; embeddings are optional.

```sh
npm install --global nestweaver@12.2.4
nestweaver index --repo /path/to/repo --db /absolute/path/to/nestweaver.lbug
nestweaver setup --db /absolute/path/to/nestweaver.lbug
nestweaver brain status --db /absolute/path/to/nestweaver.lbug --json
```

For a configured instance, keep the same `--db` binding and pass its usual
`--config /absolute/path/to/instance.toml` on commands that accept configuration.
`setup` accepts `--db`, not `--config`; its registration binds the database.
`setup` supports 16 adapters, but a written
file is not proof the client loaded it. Read its per-tool report: Aider uses the
CLI, Cline needs its settings panel, and some adapters require manual correction
(see [integration reference](https://docs.nestweaver.kehl.io/configuration/integrations/)).
Use `--force` only when intentionally replacing customized setup or rebinding an
existing database entry. Preserve unrelated agent instructions.

## Manual stdio configuration

For clients using `mcpServers`:

```json
{
  "mcpServers": {
    "nestweaver": {
      "command": "nestweaver",
      "args": ["mcp", "--db", "/absolute/path/to/nestweaver.lbug"]
    }
  }
}
```

Use an absolute executable path if the client cannot find the CLI on PATH. VS Code
uses `servers`; other clients have their own configuration format. Cursor setup
uses `--lite` with six core tools; full daemon-backed stdio advertises 43.
`--tools` can select an exact allowlist. These are local stdio commands, not an
HTTP URL; remote server configuration has separate TLS/auth requirements.

## Verify in the client

Restart or reconnect the client after setup or an upgrade. Confirm that its MCP
server is connected and inspect `tools/list`: expect 43 tools for full mode or
six for `--lite`, unless you selected an allowlist. Ask the agent to call
`brain_status`, search a known symbol with `brain_search`, and inspect a known
caller with `brain_impact`. Confirm its repository and path match the CLI graph.
A CLI command succeeding alone does not establish client MCP activation.

After upgrading a compatible graph, force-index each repo for generation 19 and
refresh each vault for Markdown derivation 3. Check `stale-check` and coverage
warnings before trusting results. Configured projects require
`nestweaver materialize-projects --config <config> --db <db>` after topology
changes; verify the expected membership with `project-context <id>` using the
same bindings. Regex acceleration is optional: `index --with-trigrams` builds it;
inspect stale/fallback disclosures rather than assuming it is present.

The npm package includes `mcpName: io.github.Kehl-io/nestweaver`. This is ownership
metadata for registry verification, not proof of a public registry listing. See
the [registry checklist](mcp-registry.md) before a separate submission.
