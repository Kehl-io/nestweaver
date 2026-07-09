#!/usr/bin/env node

const { execSync } = require("child_process");
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

if (!target) {
  console.warn(`Unsupported platform: ${key}`);
  console.warn("Supported: darwin-x64, darwin-arm64, linux-x64, linux-arm64");
  console.warn("Unsupported platform. Install from source: cargo install nestweaver");
  process.exit(0); // Don't break npm install
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

const binDir = path.join(__dirname, ".nestweaver-bin");
fs.mkdirSync(binDir, { recursive: true });
const archivePath = path.join(binDir, archive);

console.log(`Downloading NestWeaver v${version} for ${target}...`);

try {
  execSync(`curl -fsSL "${url}" -o "${archivePath}"`, { stdio: "inherit" });

  // Verify the SHA-256 the release ships alongside the archive — a mismatch
  // means a corrupted or tampered download, which must fail loudly.
  try {
    const expected = execSync(`curl -fsSL "${url}.sha256"`)
      .toString()
      .trim()
      .split(/\s+/)[0];
    const actual = execSync(`shasum -a 256 "${archivePath}"`)
      .toString()
      .trim()
      .split(/\s+/)[0];
    if (expected && actual && expected !== actual) {
      throw new Error(
        `checksum mismatch: expected ${expected}, got ${actual}`,
      );
    }
  } catch (checksumErr) {
    if (String(checksumErr.message || "").includes("checksum mismatch")) {
      throw checksumErr; // integrity failure — do not install
    }
    // Missing/unreadable .sha256 is not fatal; proceed with the download.
  }

  execSync(`tar xz -C "${binDir}" -f "${archivePath}"`, { stdio: "inherit" });
  fs.rmSync(archivePath, { force: true });
  fs.chmodSync(path.join(binDir, "nestweaver"), 0o755);
  console.log("NestWeaver installed successfully.");
} catch (err) {
  console.warn(`Failed to install NestWeaver binary: ${err.message}`);
  console.warn(`  URL: ${url}`);
  console.warn("Build from source instead: cargo install --path . (from a checkout)");
  process.exit(0); // Don't break npm install on transient/network failures
}
