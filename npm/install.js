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
  console.error(`Unsupported platform: ${key}`);
  console.error("Supported: darwin-x64, darwin-arm64, linux-x64, linux-arm64");
  console.error("Install from source: cargo install nestweaver");
  process.exit(1);
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
  console.error(`Failed to download binary from ${url}`);
  console.error("Install from source instead: cargo install nestweaver");
  process.exit(1);
}
