#!/usr/bin/env node

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const PLATFORM_MAP = {
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
};

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
const repo = "Kehl-io/nestweaver";
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
  const release = JSON.parse(
    execFileSync("curl", [
      "-fsSL",
      "-H",
      "Accept: application/vnd.github+json",
      "-H",
      "X-GitHub-Api-Version: 2026-03-10",
      releaseApiUrl,
    ]).toString(),
  );
  if (release.immutable !== true || release.tag_name !== tag) {
    throw new Error(
      `GitHub Release ${tag} is not immutable; refusing a checksum that could be replaced with its archive`,
    );
  }
  execFileSync("curl", ["-fsSL", url, "-o", archivePath], {
    stdio: "inherit",
  });

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
  execFileSync(binaryPath, ["--version"], { stdio: "inherit" });
  console.log("NestWeaver installed successfully.");
  console.log(
    "Upgrading an existing pre-publication brain? Stop its daemon and run `nestweaver publication rebuild --config /path/to/instance.toml`; the incumbent remains recoverable until validated cutover.",
  );
} catch (err) {
  fs.rmSync(archivePath, { force: true });
  // Extraction and the runtime smoke test happen after checksum validation.
  // A failure there must not leave a candidate binary that npm would expose.
  fs.rmSync(binaryPath, { force: true });
  console.error(`Failed to install NestWeaver binary: ${err.message}`);
  console.error(`  URL: ${url}`);
  console.error(
    "Download a verified GitHub Release archive, or build from a source checkout: cargo install --locked --path .",
  );
  process.exit(1);
}
