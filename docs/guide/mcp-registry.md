# Official MCP Registry preparation

Proposed listing: **NestWeaver** (`io.github.Kehl-io/nestweaver`).

> Local code knowledge graph: search symbols, trace callers, and inspect change impact across repos.

NestWeaver indexes repositories into a local graph and exposes code search,
call relationships, and change-impact tools over MCP. The listing uses the
`nestweaver` npm package and local stdio transport. Supported native platforms
are macOS and glibc Linux, on arm64 and x64; Node.js 18+ is required. No API key
is required for local indexing and these graph queries. Embeddings and remote
server configurations are optional and have their own requirements.

## Install and connect

Install the released CLI with `npm install -g nestweaver`. From the repository
root, index into a database you choose:

```sh
nestweaver index --repo . --db /absolute/path/to/nestweaver.lbug
nestweaver search "main" --db /absolute/path/to/nestweaver.lbug
```

For a client that uses `mcpServers`, configure:

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

Replace the path in every command with the same absolute path. Other clients
may use a different configuration key; see [MCP Server](../../README.md#mcp-server).
The registry manifest describes the equivalent `npx nestweaver@<version> mcp
--db <absolute-path>` launch. Clients must request the database path before
launching. The CLI starts its database daemon when necessary. Index your
repositories before querying; installing an MCP server does not index them.

## Validate before publication

```sh
node npm/nestweaver.test.js
python3 tests/support/release_mcp_registry.py --binary /absolute/path/to/nestweaver
# Also exercise a locally packed and installed npm wrapper:
python3 tests/support/release_mcp_registry.py \
  --binary /absolute/path/to/platform-package/bin/nestweaver \
  --launcher /absolute/path/to/node_modules/nestweaver/bin/nestweaver
```

The smoke test creates a disposable Git repository, isolated HOME/XDG paths,
and a unique database with an owned daemon under `/tmp/nw-release-*`. It checks
indexing, MCP initialization, tool discovery, a symbol search, and caller
impact. Evidence and daemon logs are retained there. It drains only its owned
child and checks pre-existing daemon identities remain unchanged.

Validate `server.json` against its linked official JSON Schema as well. The npm
self-test checks identity, launch arguments, and version agreement; it does not
replace schema validation or the registry's online ownership check.

## Maintainer publication checklist (separate approval required)

This preparation does not create a listing or publish an npm release.
`12.2.3` was already published without `mcpName`; its immutable npm artifact
cannot satisfy ownership verification. Release 12.2.4 includes `mcpName` and a matching manifest; verify the immutable
public package before submitting it. Publishing the CLI/npm release does not
register an MCP listing. Do not overwrite or attempt to republish `12.2.3`.

1. Review and merge this preparation through normal repository controls. Obtain
   separate approval for the npm release and registry submission.
2. Prepare a new release through the existing Release Please flow. Its extra
   file configuration updates both `server.json` version fields alongside the
   npm wrapper and all four platform packages. Confirm all versions agree and
   the release gates pass. PR events cannot invoke the release workflow; this
   change adds no publication job or credentials.
3. After approval, publish the standard platform packages and wrapper through
   the existing release process. Verify the exact new version with
   `npm view nestweaver@<new-version> version mcpName --json` and a clean packed
   install. `mcpName` must be exactly `io.github.Kehl-io/nestweaver`.
4. Re-run the smoke test against that installed release and validate the final
   manifest against the official schema. Ensure its package version exists on
   npm and contains the matching ownership metadata.
5. Install the official `mcp-publisher` using the
   [registry quickstart](https://modelcontextprotocol.io/registry/quickstart).
   From the matching release checkout, use `mcp-publisher login github` with an
   account authorized for the **Kehl-io** organization namespace. This is a
   separate registry authentication step; existing GitHub CLI access does not
   establish registry authorization. If the device flow asks for a new grant,
   have the maintainer review and approve it. Do not change organization policy
   or add credentials to this repository.
6. After explicit publication approval, run `mcp-publisher publish` from the
   repository root. Verify the exact name and version in the response and in
   `https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.Kehl-io/nestweaver`.
   Confirm the listing's command, database input, and documentation link in a
   registry-compatible client.

The GitHub namespace follows the repository owner's organization login. See
[registry authentication](https://modelcontextprotocol.io/registry/authentication)
for namespace and authorization requirements. Registry availability and grants
are checked at publication time; none are created by this preparation.
