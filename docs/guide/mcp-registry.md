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
   the release gates pass. PR events cannot invoke the release workflow. The
   separate manual registry workflow publishes metadata only after dispatch.
3. After approval, publish the standard platform packages and wrapper through
   the existing release process. Verify the exact new version with
   `npm view nestweaver@<new-version> version mcpName --json` and a clean packed
   install. `mcpName` must be exactly `io.github.Kehl-io/nestweaver`.
4. Re-run the smoke test against that installed release and validate the final
   manifest against the official schema. Ensure its package version exists on
   npm and contains the matching ownership metadata.
5. For the Kehl-io namespace, use the manual OIDC workflow described below.
   The standard interactive GitHub device login was tested and returned HTTP
   403 for organization publication: its credential cannot read organization
   roles. Owner membership alone does not make that credential sufficient.
6. Obtain separate approval to dispatch the publishing workflow. After it runs,
   verify the exact public version and latest entry, including its command,
   database input, npm version, and documentation link.

The GitHub namespace follows the repository owner's organization login. See
[registry authentication](https://modelcontextprotocol.io/registry/authentication)
for namespace and authorization requirements. Registry availability and grants
are checked at publication time; none are created by this preparation.

## Manual organization publishing with GitHub Actions OIDC

`.github/workflows/publish-mcp-registry.yml` has **only `workflow_dispatch`**.
Opening a PR, pushing a commit/tag, or publishing a GitHub release does not run
it. Merge/review and actual dispatch are separate maintainer decisions. This
workflow neither publishes npm packages nor changes the release workflow.

After review, merge, and explicit execution approval, select **Publish MCP
Registry**, choose **main**, and enter both the exact stable version and its
full release-tag commit SHA. For the first prepared listing these are:

- Version: `12.2.4`.
- Release SHA: `8e51e2ee85f248ca190f71ff5e84f5c30634f9e3`.

Run preparation without any registry login or OIDC request:

```sh
python3 scripts/test-mcp-registry-publishing.py
python3 scripts/prepare-mcp-registry-release.py \
  --version 12.2.4 --expected-sha 8e51e2ee85f248ca190f71ff5e84f5c30634f9e3 \
  --output /tmp/nestweaver-registry-validation
```

The prepare job has only `contents: read`. It checks that the tag resolves to
that SHA, the GitHub release is published and stable, and both source metadata
files come from the immutable commit. It verifies all five public npm versions,
the exact ownership name, the wrapper's SHA-512/SHA-1 integrity, and its packed
package.json against the release source. It never runs source from the release
or package scripts. Unexpected launch arguments, version ranges, tag movement,
missing packages, and conflicting registry entries fail closed.

The publishing job runs only after preparation succeeds and publication remains
necessary. It repeats preparation and binds the manifest bytes to the first
job's SHA-256, then validates with the official publisher before login. Its
only permissions are `contents: read` and **`id-token: write`**, the latter
specifically for `mcp-publisher login github-oidc`. There are no new secrets or
repository-wide permission changes. Publisher v1.8.6 is fixed to the Linux x64
archive SHA-256 `1fbe93fc0783c2394d8012c8d6e50a59e98cb2cc2d216264cc69f405d0e21340`.
The workflow itself and helper code must run from the exact NestWeaver repository
on main; the release checkout is data only. The publisher is downloaded only
from its pinned official HTTPS release URL and checksum-checked before use.

### Why this supports the organization namespace

The [official OIDC workflow guide](https://modelcontextprotocol.io/registry/github-actions)
uses `login github-oidc` with job-level `id-token: write` and no dedicated secret.
The registry's [v1.8.6 OIDC handler](https://github.com/modelcontextprotocol/registry/blob/v1.8.6/internal/api/handlers/v0/auth/github_oidc.go)
validates GitHub's signed issuer and registry audience, then `buildPermissions`
grants `io.github.<repository_owner>/*` from the verified `repository_owner`
claim. Its [organization fixture](https://github.com/modelcontextprotocol/registry/blob/v1.8.6/internal/api/handlers/v0/auth/github_oidc_test.go)
asserts `octo-org` maps to `io.github.octo-org/*`. Therefore a GitHub Actions
identity from **Kehl-io/nestweaver** maps to `io.github.Kehl-io/*`, covering the
manifest name. This is source-backed support; live token exchange/publication
still needs an approved workflow execution and has not been tested by PR CI.

The registry grants an owner namespace rather than limiting the token to this
one server or workflow. The reviewed workflow's repository/ref checks and
immutable manifest checks narrow what this workflow does; they are not a
registry-side repository/ref policy. Existing repository review and Actions
controls remain the trust boundary. No protections are changed by this workflow.

### Verification and recovery

After publication the job verifies both:

- `https://registry.modelcontextprotocol.io/v0.1/servers/io.github.Kehl-io%2Fnestweaver/versions/<version>`
- `https://registry.modelcontextprotocol.io/v0.1/servers/io.github.Kehl-io%2Fnestweaver/versions/latest`

Both must match the selected release manifest. This workflow is for the current
or a new stable release, not backfilling older versions. An already matching
exact/latest entry is a no-op and does not start the OIDC publishing job. A
conflict or a newer latest version requires investigation; it is not overwritten.
All dispatches share a non-cancelling concurrency lane. Publication has no
automatic retry: if a response is uncertain or verification fails, inspect both
public endpoints before another approved dispatch. A subsequent matching run
skips publication. The temporary registry session is logged out and removed in
an `always()` cleanup step, including after a publication failure.
