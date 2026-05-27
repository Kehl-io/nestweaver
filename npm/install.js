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
const tag = `nestweaver-v${version}`;
const archive = `nestweaver-${tag}-${target}.tar.gz`;
const url = `https://github.com/${repo}/releases/download/${tag}/${archive}`;

const binDir = path.join(__dirname, ".nestweaver-bin");
fs.mkdirSync(binDir, { recursive: true });

console.log(`Downloading NestWeaver v${version} for ${target}...`);

try {
  execSync(`curl -fsSL "${url}" | tar xz -C "${binDir}"`, {
    stdio: "inherit",
  });
  fs.chmodSync(path.join(binDir, "nestweaver"), 0o755);
  console.log("NestWeaver installed successfully.");
} catch (err) {
  console.warn(`Failed to download binary from ${url}`);
  console.warn("Install from source instead: cargo install nestweaver");
  process.exit(0); // Don't break npm install
}
