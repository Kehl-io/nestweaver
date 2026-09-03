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
  const strict = String((env && env.NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE) || "")
    .trim()
    .length > 0;
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
  const tokens = String(versionOutput || "").trim().split(/\s+/).filter(Boolean);
  if (tokens.length === 0) {
    return `the installed binary printed no version output; expected ${expected}`;
  }
  // Exact token match, so 9.0.60 can never satisfy a request for 9.0.6.
  if (tokens.includes(expected)) {
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
/// virtually every CI environment. The URL is a fixed api.github.com constant,
/// so the credential cannot be directed anywhere else.
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

module.exports = {
  assessReleaseTrust,
  runtimeVersionFailure,
  githubApiHeaders,
  describeReleaseApiFailure,
};

// ── Installation ────────────────────────────────────────────────────────

function fetchRelease(url, env) {
  const args = ["-fsSL", "--write-out", "\n%{http_code}"];
  for (const header of githubApiHeaders(env)) {
    args.push("-H", header);
  }
  args.push(url);
  let raw;
  try {
    raw = execFileSync("curl", args, { maxBuffer: 32 * 1024 * 1024 }).toString();
  } catch (error) {
    // `-f` makes curl exit non-zero on an HTTP error; recover the status from
    // whatever it managed to write so the diagnosis can be specific.
    const stdout = String((error && error.stdout) || "");
    const status = Number(stdout.trim().split("\n").pop()) || 0;
    throw new Error(
      describeReleaseApiFailure(
        { status, body: stdout.trim() || String(error.message || error) },
        env,
      ),
    );
  }
  const lines = raw.split("\n");
  const status = Number(lines.pop().trim()) || 0;
  const body = lines.join("\n");
  if (status !== 200) {
    throw new Error(describeReleaseApiFailure({ status, body }, env));
  }
  try {
    return JSON.parse(body);
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
  // binary from an older package version.
  fs.rmSync(binaryPath, { force: true });

  if (!target) {
    console.error(`Unsupported platform: ${key}`);
    console.error("Supported: darwin-x64, darwin-arm64, linux-x64, linux-arm64");
    console.error(
      "Unsupported platform. Install a verified GitHub Release archive for a supported target, or from a source checkout: cargo install --locked --path .",
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
      console.warn(`Warning: ${trust.warning}`);
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
