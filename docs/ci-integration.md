# CI Integration Guide

NestWeaver's CI integration posts **cross-repo blast radius analysis** directly
on pull requests and merge requests. When a developer changes a function
signature, renames a type, or removes an export, NestWeaver identifies every
caller across every indexed repo and comments on the PR with a severity-ranked
impact report — before the change merges.

## Quick Start: GitHub Actions

Add this workflow to `.github/workflows/nestweaver.yml`:

```yaml
name: NestWeaver Impact Analysis
on:
  pull_request:
    types: [opened, synchronize]

jobs:
  impact:
    runs-on: ubuntu-latest
    permissions:
      pull-requests: write          # required for PR comments
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0            # full history needed for diff
      - uses: Kehl-io/nestweaver/.github/actions/nestweaver-impact@v1
        with:
          server: ${{ secrets.NESTWEAVER_URL }}
          token: ${{ secrets.NESTWEAVER_TOKEN }}
```

That's it. The action diffs the PR against the base branch, sends the changed
symbols to your NestWeaver server, and posts a comment with the results.

## Quick Start: GitLab CI

Add this to your `.gitlab-ci.yml`:

```yaml
include:
  - remote: 'https://raw.githubusercontent.com/Kehl-io/nestweaver/main/ci/gitlab-template.yml'

nestweaver-impact:
  extends: .nestweaver-impact
  variables:
    NESTWEAVER_URL: $NESTWEAVER_URL
    NESTWEAVER_TOKEN: $NESTWEAVER_TOKEN
    GITLAB_TOKEN: $GITLAB_TOKEN
```

The template runs on `merge_request_event` pipelines, analyses the diff, and
posts a note on the MR.

## Required Variables / Secrets

| Variable | Platform | Description |
|---|---|---|
| `NESTWEAVER_URL` | Both | gRPC URL of the NestWeaver server (e.g. `https://nestweaver.internal:9378`) |
| `NESTWEAVER_TOKEN` | Both | API bearer token for authenticating with the server |
| `GITHUB_TOKEN` | GitHub | Automatically provided by Actions — used to post PR comments. Ensure the workflow has `pull-requests: write` permission. |
| `GITLAB_TOKEN` | GitLab | **Project Access Token** with `api` scope and Developer role or higher. See note below. |

### GitLab token gotcha

GitLab's `CI_JOB_TOKEN` **cannot** post merge request comments — the Notes API
is explicitly out of scope for job tokens (see
[GitLab issue #464591](https://gitlab.com/gitlab-org/gitlab/-/issues/464591)).
You must create a Project Access Token:

1. Go to **Settings > Access Tokens**.
2. Create a token with the **`api`** scope and **Developer** (or higher) role.
3. Add it as a **masked** CI/CD variable named `GITLAB_TOKEN`.

## Configuration Options

Both the GitHub Action and GitLab template accept two optional flags that
control when the CI job exits non-zero:

### `fail-on-breaking`

Only fails the job when the impact report contains **BREAKING** severity
changes — a removed function still called elsewhere, a changed signature with
downstream callers, etc. Server errors (unreachable server, timeout) still
exit 0 so a NestWeaver outage doesn't block your entire pipeline.

**Use case:** Gate merges on cross-repo breaking changes without coupling
pipeline availability to the NestWeaver server.

GitHub Actions:
```yaml
- uses: Kehl-io/nestweaver/.github/actions/nestweaver-impact@v1
  with:
    server: ${{ secrets.NESTWEAVER_URL }}
    token: ${{ secrets.NESTWEAVER_TOKEN }}
    fail-on-breaking: true
```

GitLab CI:
```yaml
nestweaver-impact:
  extends: .nestweaver-impact
  variables:
    NESTWEAVER_FAIL_ON_BREAKING: "true"
```

### `fail-on-error`

Fails the job if the NestWeaver server is unreachable or returns an error.
Opt-in strict mode for teams that require impact analysis on every PR and
treat a missing report as a pipeline failure.

GitHub Actions:
```yaml
- uses: Kehl-io/nestweaver/.github/actions/nestweaver-impact@v1
  with:
    server: ${{ secrets.NESTWEAVER_URL }}
    token: ${{ secrets.NESTWEAVER_TOKEN }}
    fail-on-error: true
```

GitLab CI:
```yaml
nestweaver-impact:
  extends: .nestweaver-impact
  variables:
    NESTWEAVER_FAIL_ON_ERROR: "true"
```

### Defaults

Both flags default to `false`. With defaults, a server outage logs a warning
and the job exits 0 — the PR is never blocked by NestWeaver infrastructure
issues.

### Additional GitLab options

The GitLab template also accepts `NESTWEAVER_MIN_SEVERITY` to filter the
report. Valid values: `breaking`, `warning`, `info` (default: `info`).

### GitLab Code Quality report (MR widget)

The GitLab template also emits a **Code Quality** report
(`gl-code-quality-report.json`, in the CodeClimate format) and wires it via
`artifacts.reports.codequality`, so impacted callers surface as inline
annotations in the merge-request **Code Quality** widget — in addition to the
posted comment. It is generated from `impact.json` and is **always** written
(an empty `[]` when the server is unavailable or there are no impacts), so the
report reference never dangles.

- **Severity map:** Breaking → `critical`, Warning → `major`, Info → `info`.
- **`check_name`:** `nestweaver/impact`.
- **`fingerprint`:** a stable blake3 hash of the affected + change canonical IDs
  and the affected file, so GitLab deduplicates and tracks the same finding
  across commits.
- **`location`:** the affected caller's repo-relative file and line
  (`lines.begin` clamped to ≥ 1).

To generate the report yourself:

```sh
nestweaver format-comment --input impact.json --codequality-out gl-code-quality-report.json
```

## Example PR Comment Output

When the action runs, it posts (or updates) a comment like this:

```markdown
## NestWeaver Impact Analysis

| Severity | Count | Repos Affected |
|----------|-------|----------------|
| BREAKING | 3     | billing-service, admin-dashboard |
| WARNING  | 2     | notification-service |

<details>
<summary>BREAKING: processPayment — signature changed (3 callers)</summary>

| Caller | File | Line | Issue |
|--------|------|------|-------|
| billing-service | src/webhook.rs | 42 | Missing parameter `idempotencyKey` |
| admin-dashboard | src/refund.tsx | 118 | Missing parameter `idempotencyKey` |
| billing-service | src/retry.rs | 87 | Missing parameter `idempotencyKey` |

</details>

<details>
<summary>WARNING: PaymentStatus — new variant added (2 consumers)</summary>

| Consumer | File | Line | Issue |
|----------|------|------|-------|
| notification-service | src/handlers/payment.rs | 34 | Non-exhaustive match on PaymentStatus |
| notification-service | src/templates.rs | 91 | Non-exhaustive match on PaymentStatus |

</details>
```

The comment is updated in-place on subsequent pushes (identified by a hidden
marker), so the PR never accumulates duplicate comments.

For large PRs, the comment shows the top 50 symbols by severity. The full
report is always available as a CI artifact (`impact.json`, retained for 30
days).

## Networking

### GitHub-hosted runners

GitHub-hosted runners run on GitHub's infrastructure and need to reach your
NestWeaver server over the public internet. Recommended setup:

- Expose the NestWeaver gRPC port behind TLS (e.g. via a load balancer or
  reverse proxy).
- Authenticate with a bearer token (`NESTWEAVER_TOKEN`).
- Restrict access by IP range if desired — GitHub publishes their runner IP
  ranges via the [meta API](https://api.github.com/meta).

### Self-hosted runners (GitHub or GitLab)

Self-hosted runners typically share a network with the NestWeaver server.
Point `NESTWEAVER_URL` at the internal address (e.g.
`http://nestweaver.internal:9378`) — no TLS or public exposure required.

### GitLab CI

GitLab CI runners are usually self-hosted and already on the same network as
the NestWeaver server. If you use GitLab SaaS with shared runners, the same
public-facing TLS setup as GitHub-hosted runners applies.

## Troubleshooting

### Server unreachable

- Verify `NESTWEAVER_URL` is correct and includes the port.
- Check firewall rules between the CI runner and the NestWeaver server.
- If using TLS, confirm the certificate is valid and the runner trusts the CA.
- Enable `fail-on-error: true` temporarily to surface connection errors as
  job failures instead of silent warnings.

### Token issues

- Confirm `NESTWEAVER_TOKEN` is set and not expired.
- Check the token has the correct scope for impact analysis.
- Ensure the secret is not accidentally masked in a way that truncates it
  (GitLab masks values shorter than 8 characters).

### No comment posted (GitHub)

- The workflow must have `pull-requests: write` permission. Add the
  `permissions` block shown in the Quick Start example.
- If the repo is in an organization with restricted default permissions,
  explicitly grant the permission in the workflow file.

### No comment posted (GitLab)

- `CI_JOB_TOKEN` will **not** work — use a Project Access Token with `api`
  scope. See the [GitLab token gotcha](#gitlab-token-gotcha) section above.
- Verify the token's role is Developer or higher.

### Large PRs truncated

The PR comment shows the top 50 symbols ranked by severity. The full
untruncated report is saved as a CI artifact (`impact.json`). Download it
from the job's artifact page or via the API for programmatic consumption.

### Pipeline blocked unexpectedly

If the job is failing when you don't expect it to, check which flags are
enabled. With `fail-on-breaking: true`, any BREAKING impact will fail the job.
With `fail-on-error: true`, server connectivity issues will also fail it.
Set both to `false` (the default) while debugging.
