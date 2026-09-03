#!/usr/bin/env node

// Behavioral release gate for the npm postinstall wrapper. It runs the real
// candidate install.js against deterministic fake curl/checksum/tar tools so
// success, missing-checksum, and checksum-mismatch behavior are verified
// without touching the network.

const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const sourcePackage = path.resolve(process.argv[2] || "npm");
if (!fs.existsSync(path.join(sourcePackage, "install.js"))) {
  throw new Error(`candidate npm package is missing install.js: ${sourcePackage}`);
}

const archiveBytes = Buffer.from("verified-release-archive\n");
const archiveSha = crypto.createHash("sha256").update(archiveBytes).digest("hex");
const fixtureTool = `#!/usr/bin/env node
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const tool = path.basename(process.argv[1]);
const args = process.argv.slice(2);
const bytes = Buffer.from("verified-release-archive\\n");
const sha = crypto.createHash("sha256").update(bytes).digest("hex");
if (tool === "curl") {
  if (args.some((arg) => arg.includes("api.github.com/repos/") && arg.includes("/releases/tags/"))) {
    const immutable = !String(process.env.NW_INSTALL_TEST_MODE || "").startsWith(
      "mutable-release",
    );
    const version = JSON.parse(fs.readFileSync(path.join(process.cwd(), "package.json"))).version;
    // The installer asks curl to append the HTTP status on its own line and
    // parses that last line, so a fake that omits it is not modelling the call.
    process.stdout.write(JSON.stringify({ immutable, tag_name: "v" + version }) + "\\n200");
    process.exit(0);
  }
  const outputAt = args.indexOf("-o");
  if (outputAt >= 0) {
    fs.writeFileSync(args[outputAt + 1], bytes);
    process.exit(0);
  }
  if (process.env.NW_INSTALL_TEST_MODE === "missing-checksum") process.exit(22);
  const rendered = process.env.NW_INSTALL_TEST_MODE === "checksum-mismatch"
    ? "0".repeat(64)
    : sha;
  process.stdout.write(rendered + "  archive.tar.gz\\n");
  process.exit(0);
}
if (tool === "shasum") {
  process.stdout.write(sha + "  archive.tar.gz\\n");
  process.exit(0);
}
if (tool === "tar") {
  const destination = args[args.indexOf("-C") + 1];
  const mode = process.env.NW_INSTALL_TEST_MODE;
  const exitCode = mode === "runtime-failure" ? 7 : 0;
  const version = JSON.parse(fs.readFileSync(path.join(process.cwd(), "package.json"))).version;
  // The installer captures --version and matches it against the package
  // version, so the fake binary has to report one. A fake that printed nothing
  // would fail the success case for the wrong reason.
  const reported = mode === "version-mismatch" ? "0.0.0-not-this-build" : version;
  fs.writeFileSync(
    path.join(destination, "nestweaver"),
    '#!/bin/sh\\necho "nestweaver ' + reported + '"\\nexit ' + exitCode + "\\n",
    { mode: 0o755 },
  );
  process.exit(0);
}
throw new Error("unexpected fixture tool: " + tool);
`;

const root = fs.mkdtempSync(path.join(os.tmpdir(), "nestweaver-install-gate-"));
try {
  const fakeBin = path.join(root, "bin");
  fs.mkdirSync(fakeBin);
  for (const tool of ["curl", "shasum", "tar"]) {
    fs.writeFileSync(path.join(fakeBin, tool), fixtureTool, { mode: 0o755 });
  }

  function runCase(mode, expectedSuccess) {
    const candidate = path.join(root, mode);
    fs.cpSync(sourcePackage, candidate, { recursive: true });
    const binary = path.join(candidate, ".nestweaver-bin", "nestweaver");
    fs.mkdirSync(path.dirname(binary), { recursive: true });
    fs.writeFileSync(binary, "stale binary from an older package\n", { mode: 0o755 });
    const result = spawnSync(process.execPath, [path.join(candidate, "install.js")], {
      cwd: candidate,
      env: {
        ...process.env,
        PATH: `${fakeBin}${path.delimiter}${process.env.PATH || ""}`,
        NW_INSTALL_TEST_MODE: mode,
        NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE:
          mode === "mutable-release-strict" ? "1" : "",
      },
      encoding: "utf8",
    });
    if (expectedSuccess) {
      if (result.status !== 0 || !fs.existsSync(binary)) {
        throw new Error(
          `postinstall success case failed (status ${result.status}):\n${result.stdout}\n${result.stderr}`,
        );
      }
      if ((fs.statSync(binary).mode & 0o111) === 0) {
        throw new Error("postinstall success did not leave an executable binary");
      }
    } else if (result.status === 0 || fs.existsSync(binary)) {
      throw new Error(
        `postinstall ${mode} case did not fail closed (status ${result.status}):\n${result.stdout}\n${result.stderr}`,
      );
    }
  }

  runCase("success", true);
  runCase("missing-checksum", false);
  runCase("checksum-mismatch", false);
  // A mutable release is DISCLOSED, not refused. Every release published
  // before immutable releases were enabled reports `immutable: false`, and
  // 9.0.5 -- the only published version -- is one of them, so refusing here
  // would break the only install that exists. The checksum is still enforced,
  // which the two cases above gate.
  runCase("mutable-release", true);
  // ...and the strict opt-in still refuses, so the tightened contract is
  // gated rather than merely documented.
  runCase("mutable-release-strict", false);
  // The installed binary must BE the version the package claims.
  runCase("version-mismatch", false);
  runCase("runtime-failure", false);
  process.stdout.write(`npm postinstall behavioral gate passed (${archiveSha})\n`);
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
