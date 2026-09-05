#!/usr/bin/env node
"use strict";

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const PLATFORM_MAP = {
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
};

const repo = "Kehl-io/nestweaver";

// ── Decision logic ──────────────────────────────────────────────────────
// Pure, exported, and covered by install.test.js. Everything below the
// `main()` boundary does I/O and is not directly testable; keeping the
// judgements out here is what makes the installer assertable at all.

/// Explicit boolean parsing for operator-facing env vars. `0`, `false`, `no`,
/// `off` and blank all mean OFF; anything else non-blank means ON.
function isTruthyEnv(value) {
  const normalized = String(value || "").trim().toLowerCase();
  if (normalized.length === 0) {
    return false;
  }
  return !["0", "false", "no", "off"].includes(normalized);
}

/// Whether a GitHub Release may be trusted to anchor its own checksum.
///
/// The checksum lives beside the archive in the same release, so an actor who
/// can replace release assets can replace both. Only an IMMUTABLE release makes
/// the checksum tamper-evidence rather than a self-signed claim.
///
/// A wrong tag is always fatal. A non-immutable release is DISCLOSED, not
/// refused: immutable releases were enabled on this repository only recently,
/// so every version already on the npm registry reports `immutable: false`
/// (verified against the live API for v9.0.0, v9.0.4 and v9.0.5). Refusing them
/// would make `npm install nestweaver` fail for every published version --
/// a self-inflicted outage, not a security control. The checksum is still
/// fetched and still enforced; what is absent is tamper-evidence, and saying so
/// is the honest form. `NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE=1` opts into the
/// strict contract for callers who want it.
function assessReleaseTrust(release, tag, env) {
  const actualTag = release && release.tag_name;
  if (actualTag !== tag) {
    return {
      failure: `GitHub Release tag ${JSON.stringify(actualTag)} does not match the requested ${tag}`,
      warning: null,
    };
  }
  if (release.immutable === true) {
    return { failure: null, warning: null };
  }
  // Any non-blank value used to mean "strict", so an operator writing
  // NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE=0 to opt OUT was opted IN and every
  // build failed.
  const strict = isTruthyEnv(env && env.NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE);
  const detail =
    `GitHub Release ${tag} is not immutable, so its published SHA-256 could have been ` +
    `replaced together with its archive. The checksum is still verified, but it is not ` +
    `tamper-evidence. Releases published before immutable releases were enabled on this ` +
    `repository all report this.`;
  if (strict) {
    return {
      failure: `${detail} NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE is set, so this install is refused.`,
      warning: null,
    };
  }
  return {
    failure: null,
    warning: `${detail} Set NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE=1 to refuse instead of warn.`,
  };
}

/// Whether the binary that was just installed IS the version this package
/// claims. Executing `--version` only proves the file runs; comparing its
/// output is what proves identity, and the previous implementation piped that
/// output straight to the terminal and compared nothing.
function runtimeVersionFailure(versionOutput, expected) {
  // First line only, and the version must be reported AS the version -- i.e.
  // `<name> <version>`. Accepting the token anywhere in the output let prose
  // such as "9.0.6 is not this build; actual 1.0.0" pass.
  const firstLine = String(versionOutput || "").trim().split(/\r?\n/)[0] || "";
  const tokens = firstLine.split(/\s+/).filter(Boolean);
  if (tokens.length === 0) {
    return `the installed binary printed no version output; expected ${expected}`;
  }
  // The version must sit where a version goes: `<name> <version>` (the shape
  // `nestweaver --version` actually prints), or a bare version on its own.
  // Exact token match, so 9.0.60 can never satisfy a request for 9.0.6.
  const reported = tokens.length === 1 ? tokens[0] : tokens[1];
  if (reported === expected) {
    return null;
  }
  return `the installed binary reports ${JSON.stringify(
    String(versionOutput).trim(),
  )}, which does not contain the expected version ${expected}`;
}

/// Headers for the release-metadata call.
///
/// Unauthenticated api.github.com allows 60 requests/hour/IP, so a shared CI
/// egress or a corporate NAT exhausts it and every install behind that address
/// fails. A token raises the ceiling to 5,000 and is already present in
/// virtually every CI environment.
///
/// TWO SEPARATE PROPERTIES, and only one of them is about the URL. The URL is a
/// fixed api.github.com constant, so the credential cannot be sent anywhere
/// else. But the credential must also not be EXPOSED where it is sent from:
/// passing it as a curl argv element publishes it to every local user through
/// the process table (`/proc/<pid>/cmdline` is world-readable on Linux, which
/// is what CI runners are), so `fetchRelease` feeds these to curl on stdin via
/// `--config -` rather than as `-H` arguments.
function githubApiHeaders(env) {
  const headers = [
    "Accept: application/vnd.github+json",
    "X-GitHub-Api-Version: 2026-03-10",
  ];
  const token = [env && env.GITHUB_TOKEN, env && env.GH_TOKEN]
    .map((value) => String(value || "").trim())
    .find((value) => value.length > 0);
  if (token) {
    headers.push(`Authorization: Bearer ${token}`);
  }
  return headers;
}

/// Turn a failed release-metadata call into something the caller can act on.
/// Failing closed is correct; failing closed without naming the remedy is what
/// makes a rate limit look like a broken package.
function describeReleaseApiFailure(failure, env) {
  const status = failure && failure.status;
  const body = String((failure && failure.body) || "").trim();
  const authenticated = githubApiHeaders(env).some((header) =>
    header.startsWith("Authorization"),
  );
  if (status === 401) {
    return authenticated
      ? `GitHub rejected the credential in GITHUB_TOKEN/GH_TOKEN (HTTP 401: ${body}). This endpoint needs no credential for a public repository -- unset that variable, or replace the expired or wrong-scope token.`
      : `GitHub returned HTTP 401 for an unauthenticated request: ${body}`;
  }
  if ((status === 403 || status === 429) && /rate limit/i.test(body)) {
    return authenticated
      ? `the GitHub API rate limit was exhausted for the credential in use; wait for the limit to reset and retry: ${body}`
      : `the GitHub API rate limit was exhausted for this IP address (unauthenticated callers get 60 requests/hour, and a shared CI or NAT egress exhausts that quickly). Export GITHUB_TOKEN or GH_TOKEN to raise it, or wait for the limit to reset: ${body}`;
  }
  if (status === 404) {
    return `the GitHub Release was not found (HTTP 404); the tag may not be published yet: ${body}`;
  }
  return `could not read GitHub Release metadata${status ? ` (HTTP ${status})` : ""}: ${body}`;
}

/// Whether the running system is musl (Alpine and similar), not glibc.
///
/// nw-433: npm's package-level `libc` field cannot express "glibc-only on
/// Linux, unconstrained everywhere else" -- it constrains the WHOLE package,
/// so declaring `libc: ["glibc"]` to gate the two Linux targets also refused
/// every macOS install, where `libc` is not a concept at all and npm reports
/// `Actual libc: undefined`. The field has been removed from package.json;
/// this function is what replaces it, checked here in code where "Linux, and
/// only Linux" is expressible, instead of in metadata where it is not.
///
/// `glibcVersionRuntime` is populated by Node's own `process.report` only when
/// Node itself is linked against glibc (using glibc's `gnu_get_libc_version`
/// at compile time); a musl-linked Node -- the system Node.js on Alpine, and
/// every `node:*-alpine` / `node:*-musl` image -- never has it. Node running
/// under musl is a reliable proxy for the host being musl: nothing supported
/// here runs a glibc Node atop a musl userland. Takes the already-extracted
/// value rather than `process.report` itself so it is testable without
/// stubbing a global.
function isMuslLinux(platform, glibcVersionRuntime) {
  return platform === "linux" && !glibcVersionRuntime;
}

module.exports = {
  assessReleaseTrust,
  runtimeVersionFailure,
  githubApiHeaders,
  describeReleaseApiFailure,
  isMuslLinux,
};

// ── Installation ────────────────────────────────────────────────────────

/// One release-metadata request. Returns `{ status, body }`; never throws for
/// an HTTP error, so the caller can decide whether to retry.
///
/// `--fail-with-body`, NOT `-f`. Plain `-f` suppresses the response body on an
/// HTTP error, which left `describeReleaseApiFailure` matching /rate limit/
/// against a body that could only ever be the three-digit status -- so the
/// rate-limit branch, the entire reason the token handling exists, was
/// unreachable. Verified: with `-f` the recovered body is the literal "404".
///
/// Headers go over stdin via `--config -` so a bearer token never appears in
/// this process's argv.
function requestRelease(url, headers) {
  // The URL stays a normal argument: it is not a secret, and keeping it in
  // argv is what lets tooling (and the release gate's fake curl) see which
  // request is being made. ONLY the headers go over stdin, because that is
  // where the bearer token lives.
  const args = [
    "--fail-with-body",
    "-sSL",
    "--write-out",
    "\n%{http_code}",
    "--config",
    "-",
    url,
  ];
  const config = headers
    .map((header) => `header = ${JSON.stringify(header)}`)
    .join("\n");
  let raw;
  try {
    raw = execFileSync("curl", args, {
      input: config,
      maxBuffer: 32 * 1024 * 1024,
    }).toString();
  } catch (error) {
    const stdout = String((error && error.stdout) || "");
    if (stdout.length === 0) {
      // curl never ran, or died before writing: ENOENT / timeout / DNS.
      // `error.message` is safe to surface here because these arms carry no
      // argv -- and argv no longer carries the token in any case.
      return { status: 0, body: String((error && error.message) || error) };
    }
    raw = stdout;
  }
  const lines = raw.split("\n");
  const status = Number(lines.pop().trim()) || 0;
  return { status, body: lines.join("\n").trim() };
}

function fetchRelease(url, env) {
  const headers = githubApiHeaders(env);
  const authenticated = headers.some((header) => header.startsWith("Authorization"));
  let result = requestRelease(url, headers);

  // This endpoint needs no credential for a public repository, so an ambient
  // GITHUB_TOKEN/GH_TOKEN can only ever make it worse: `gh auth login` and
  // composite workflows routinely export a stale or wrong-scope token, which
  // turns a request that would have succeeded anonymously into a hard 401.
  // Retry once without it rather than failing on a credential the caller never
  // asked us to use. Rate-limited responses are NOT retried -- dropping the
  // token there would make the limit stricter, not looser.
  if (
    authenticated &&
    (result.status === 401 || (result.status === 403 && !/rate limit/i.test(result.body)))
  ) {
    const anonymous = headers.filter((header) => !header.startsWith("Authorization"));
    const retry = requestRelease(url, anonymous);
    if (retry.status === 200) {
      result = retry;
    }
  }

  if (result.status !== 200) {
    throw new Error(describeReleaseApiFailure(result, env));
  }
  try {
    return JSON.parse(result.body);
  } catch (error) {
    throw new Error(`GitHub Release metadata was not valid JSON: ${error.message}`);
  }
}

function main() {
  const key = `${process.platform}-${process.arch}`;
  const target = PLATFORM_MAP[key];
  const binDir = path.join(__dirname, ".nestweaver-bin");
  const binaryPath = path.join(binDir, "nestweaver");
  fs.mkdirSync(binDir, { recursive: true });
  // Never let a failed upgrade leave the newly installed wrapper executing a
  // binary from an older package version, or a previous run's disclosure
  // describing a release this run did not install.
  fs.rmSync(binaryPath, { force: true });
  fs.rmSync(path.join(binDir, "RELEASE-NOT-IMMUTABLE.txt"), { force: true });

  if (!target) {
    console.error(`Unsupported platform: ${key}`);
    console.error("Supported: darwin-x64, darwin-arm64, linux-x64, linux-arm64");
    console.error(
      "Unsupported platform. Install a verified GitHub Release archive for a supported target, or from a source checkout: cargo install --locked --path .",
    );
    process.exit(1);
  }

  // The two Linux targets published (see PLATFORM_MAP above) are both
  // `-unknown-linux-gnu`: glibc-linked. There is no musl target, so
  // downloading one for a musl system (Alpine, `node:*-alpine`) would fetch a
  // binary that cannot dynamically link and fail confusingly, well after this
  // point, with no indication the platform was the problem. Reject it here,
  // before any network call, with the same actionable shape as the `!target`
  // case above -- this is the check that used to live (incorrectly, for every
  // platform including macOS) in package.json's `libc` field. See nw-433.
  let glibcVersionRuntime;
  try {
    glibcVersionRuntime =
      process.report && typeof process.report.getReport === "function"
        ? process.report.getReport().header.glibcVersionRuntime
        : undefined;
  } catch {
    // Report generation is best-effort; an environment where it throws gets
    // the benefit of the doubt rather than a false rejection.
    glibcVersionRuntime = undefined;
  }
  if (isMuslLinux(process.platform, glibcVersionRuntime)) {
    console.error(`Unsupported platform: ${key} (musl libc)`);
    console.error(
      "NestWeaver's published Linux releases are glibc-only (x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu). musl/Alpine is not currently supported.",
    );
    console.error(
      "Build from a source checkout instead: cargo install --locked --path .",
    );
    process.exit(1);
  }

  const version = require("./package.json").version;
  // Release tags are `v<version>` (release-please config: include-component-in-tag
  // false) and assets are `nestweaver-v<version>-<target>.tar.gz` (see the
  // `build` job in .github/workflows/release-please.yml). Keep these in sync.
  const tag = `v${version}`;
  const archive = `nestweaver-${tag}-${target}.tar.gz`;
  const base = `https://github.com/${repo}/releases/download/${tag}`;
  const url = `${base}/${archive}`;
  const releaseApiUrl = `https://api.github.com/repos/${repo}/releases/tags/${tag}`;
  const archivePath = path.join(binDir, archive);

  console.log(`Downloading NestWeaver v${version} for ${target}...`);

  try {
    const trust = assessReleaseTrust(
      fetchRelease(releaseApiUrl, process.env),
      tag,
      process.env,
    );
    if (trust.failure) {
      throw new Error(trust.failure);
    }
    if (trust.warning) {
      // npm >= 7 HIDES lifecycle-script output unless the script fails or
      // --foreground-scripts is passed, so a console warning alone does not
      // reach an ordinary `npm install` user -- which would make "disclosed"
      // a claim this code does not deliver. Leave it on disk beside the binary
      // as well, so the disclosure is greppable after the fact.
      console.warn(`Warning: ${trust.warning}`);
      try {
        fs.writeFileSync(
          path.join(binDir, "RELEASE-NOT-IMMUTABLE.txt"),
          `${trust.warning}\n\nRelease: ${tag}\nArchive: ${url}\n`,
        );
      } catch {
        // Best effort: a disclosure that cannot be written must not fail an
        // install whose checksum verified.
      }
    }

    execFileSync("curl", ["-fsSL", url, "-o", archivePath], { stdio: "inherit" });

    // Verify the SHA-256 the release ships alongside the archive — a mismatch
    // means a corrupted or tampered download. Missing, unreadable, or malformed
    // checksum evidence is equally non-authoritative and must fail closed.
    const expected = execFileSync("curl", ["-fsSL", `${url}.sha256`])
      .toString()
      .trim()
      .split(/\s+/)[0];
    const actual = execFileSync("shasum", ["-a", "256", archivePath])
      .toString()
      .trim()
      .split(/\s+/)[0];
    if (!/^[0-9a-f]{64}$/i.test(expected) || !/^[0-9a-f]{64}$/i.test(actual)) {
      throw new Error("release checksum evidence is missing or malformed");
    }
    if (expected.toLowerCase() !== actual.toLowerCase()) {
      throw new Error(`checksum mismatch: expected ${expected}, got ${actual}`);
    }

    execFileSync("tar", ["xz", "-C", binDir, "-f", archivePath], {
      stdio: "inherit",
    });
    if (!fs.existsSync(binaryPath) || !fs.statSync(binaryPath).isFile()) {
      throw new Error("verified archive did not contain the nestweaver executable");
    }
    fs.rmSync(archivePath, { force: true });
    fs.chmodSync(binaryPath, 0o755);

    // Capture, do not inherit: piping `--version` to the terminal proves the
    // binary executes and compares nothing.
    const reported = execFileSync(binaryPath, ["--version"]).toString();
    const versionFailure = runtimeVersionFailure(reported, version);
    if (versionFailure) {
      throw new Error(versionFailure);
    }
    console.log(`NestWeaver installed successfully (${reported.trim()}).`);
    console.log(
      "Upgrading an existing pre-publication brain? Stop its daemon and run `nestweaver publication rebuild --config /path/to/instance.toml`; the incumbent remains recoverable until validated cutover.",
    );
  } catch (err) {
    fs.rmSync(archivePath, { force: true });
    // Extraction and the runtime identity check happen after checksum
    // validation. A failure there must not leave a candidate binary that npm
    // would expose.
    fs.rmSync(binaryPath, { force: true });
    console.error(`Failed to install NestWeaver binary: ${err.message}`);
    console.error(`  URL: ${url}`);
    console.error(
      "Download a verified GitHub Release archive, or build from a source checkout: cargo install --locked --path .",
    );
    process.exit(1);
  }
}

// Only install when executed as the postinstall script; `require` must be free
// of side effects so the decision logic above can be tested at all.
if (require.main === module) {
  main();
}
