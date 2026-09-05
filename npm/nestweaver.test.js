#!/usr/bin/env node
"use strict";

// Unit + integration tests for bin/nestweaver's decision logic.
//
// nw-425 / nw-433: this package's `install.js` (a `postinstall` that
// downloaded the matching GitHub Release archive at install time) is
// retired. pnpm 10+ blocks lifecycle scripts by default, so `pnpm add
// nestweaver` produced a wrapper with no binary and no clear reason why. The
// package now ships one optionalDependency per platform (the esbuild/swc
// pattern) with NO lifecycle script anywhere, and `bin/nestweaver` resolves
// and execs the platform package's binary at INVOCATION time instead.
//
// Everything install.test.js used to cover (release-metadata trust,
// checksum-adjacent runtime version checks, GitHub API auth headers, rate
// limit diagnosis) tested code that downloaded and verified a release
// archive. There is no longer a download at install or invocation time --
// npm/pnpm/yarn's own optionalDependencies resolution is what selects and
// fetches the right platform package, verified by their own package
// integrity checks -- so that code and those tests are gone, not ported.
// `isMuslLinux` is the one piece of decision logic that survives unchanged
// (Linux/glibc detection is still meaningful at invocation time) and its
// tests are carried over verbatim.
//
// Run directly (`node nestweaver.test.js`); Required CI invokes it.

const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const {
  PLATFORM_PACKAGES,
  platformKey,
  platformPackageName,
  isMuslLinux,
  unsupportedPlatformMessage,
  muslMessage,
  missingPlatformPackageMessage,
  resolveBinaryPath,
} = require("./bin/nestweaver");

let passed = 0;
function check(name, fn) {
  fn();
  passed += 1;
  console.log(`  ok - ${name}`);
}

console.log("bin/nestweaver self-test");

// ── Platform → package mapping ──────────────────────────────────────────
check("all four published targets map to their platform package", () => {
  assert.strictEqual(platformPackageName("darwin", "arm64"), "nestweaver-darwin-arm64");
  assert.strictEqual(platformPackageName("darwin", "x64"), "nestweaver-darwin-x64");
  assert.strictEqual(platformPackageName("linux", "arm64"), "nestweaver-linux-arm64");
  assert.strictEqual(platformPackageName("linux", "x64"), "nestweaver-linux-x64");
});

check("an unsupported platform maps to null, not a guess", () => {
  assert.strictEqual(platformPackageName("win32", "x64"), null);
  assert.strictEqual(platformPackageName("darwin", "ia32"), null);
  assert.strictEqual(platformPackageName("linux", "ia32"), null);
});

check("PLATFORM_PACKAGES has exactly the four published targets", () => {
  assert.deepStrictEqual(Object.keys(PLATFORM_PACKAGES).sort(), [
    "darwin-arm64",
    "darwin-x64",
    "linux-arm64",
    "linux-x64",
  ]);
});

check("platformKey composes platform and arch the same way PLATFORM_PACKAGES is keyed", () => {
  assert.strictEqual(platformKey("linux", "x64"), "linux-x64");
});

// ── Messages name the real remedy ───────────────────────────────────────
check("the unsupported-platform message lists the four real targets", () => {
  const message = unsupportedPlatformMessage("win32", "x64");
  assert.match(message, /win32-x64/);
  for (const name of Object.values(PLATFORM_PACKAGES)) {
    const shortName = name.replace("nestweaver-", "");
    assert.ok(message.includes(shortName), `expected ${shortName} in: ${message}`);
  }
  assert.match(message, /cargo install/);
});

check("the musl message names the two glibc Linux packages, not a generic platform list", () => {
  const message = muslMessage("linux", "x64");
  assert.match(message, /musl/i);
  assert.match(message, /nestweaver-linux-x64/);
  assert.match(message, /nestweaver-linux-arm64/);
});

check("the missing-optional-dependency message names the exact package and version to install", () => {
  const message = missingPlatformPackageMessage("nestweaver-darwin-arm64", "9.1.0");
  assert.match(message, /nestweaver-darwin-arm64@9\.1\.0/);
  assert.match(message, /optional/i);
});

// ── libc detection (nw-433), carried over unchanged from install.js ────
check("darwin is never musl, regardless of glibcVersionRuntime", () => {
  assert.strictEqual(isMuslLinux("darwin", undefined, false), false);
  assert.strictEqual(isMuslLinux("darwin", "2.31", true), false);
});

check("linux with a reported glibc version is not musl", () => {
  assert.strictEqual(isMuslLinux("linux", "2.31", true), false);
});

check("linux with a report that reads no glibc version IS musl", () => {
  assert.strictEqual(isMuslLinux("linux", undefined, true), true);
});

check("linux with an UNAVAILABLE report is NOT treated as musl (fails open)", () => {
  assert.strictEqual(isMuslLinux("linux", undefined, false), false);
});

// ── Binary resolution ────────────────────────────────────────────────────
// `resolveBinaryPath` takes the resolver as a parameter specifically so this
// is testable without a real installed optionalDependency tree.
check("resolveBinaryPath returns the resolver's path on success", () => {
  const { binaryPath, error } = resolveBinaryPath("nestweaver-linux-x64", (specifier) => {
    assert.strictEqual(specifier, "nestweaver-linux-x64/bin/nestweaver");
    return "/fake/node_modules/nestweaver-linux-x64/bin/nestweaver";
  });
  assert.strictEqual(error, null);
  assert.strictEqual(binaryPath, "/fake/node_modules/nestweaver-linux-x64/bin/nestweaver");
});

check("resolveBinaryPath surfaces (not throws) a resolution failure", () => {
  const boom = new Error("Cannot find module 'nestweaver-linux-x64/bin/nestweaver'");
  const { binaryPath, error } = resolveBinaryPath("nestweaver-linux-x64", () => {
    throw boom;
  });
  assert.strictEqual(binaryPath, null);
  assert.strictEqual(error, boom);
});

// ── End-to-end: fake optionalDependency tree, no compiled binary needed ──
// Proves the actual seam nw-425 broke: invoking `bin/nestweaver` as a real
// child process must select and exec the platform package matching THIS
// process's platform/arch, passing arguments and exit code through
// faithfully -- and must NOT silently succeed when only the WRONG platform's
// package is present. A fake shell/batch "binary" stands in for the real
// compiled release binary so this runs in CI without cargo.
function makeFakeNodeModulesTree(rootDir, installedPackageNames) {
  const nodeModules = path.join(rootDir, "node_modules");
  fs.mkdirSync(nodeModules, { recursive: true });
  // Mirror the real install layout: the launcher itself lives at
  // node_modules/nestweaver/bin/nestweaver, one level below where its own
  // optionalDependency siblings (node_modules/nestweaver-<platform>) sit.
  // This matters for two reasons the test would otherwise miss: (1) the
  // launcher reads its OWN version from "../package.json" relative to
  // itself, and (2) Node's module resolution walks up from the launcher's
  // own directory, so placing it anywhere else changes what it can see.
  fs.mkdirSync(path.join(nodeModules, "nestweaver", "bin"), { recursive: true });
  fs.writeFileSync(
    path.join(nodeModules, "nestweaver", "package.json"),
    JSON.stringify({ name: "nestweaver", version: "9.1.0" }),
  );
  for (const name of installedPackageNames) {
    const pkgDir = path.join(nodeModules, name);
    fs.mkdirSync(path.join(pkgDir, "bin"), { recursive: true });
    fs.writeFileSync(
      path.join(pkgDir, "package.json"),
      JSON.stringify({ name, version: "9.1.0" }),
    );
    const binPath = path.join(pkgDir, "bin", "nestweaver");
    fs.writeFileSync(
      binPath,
      "#!/usr/bin/env node\nconsole.log('FAKE_BINARY:' + JSON.stringify(process.argv.slice(2)));\nprocess.exit(process.argv.includes('--fail') ? 7 : 0);\n",
    );
    fs.chmodSync(binPath, 0o755);
  }
}

function currentPlatformPackage() {
  const name = platformPackageName(process.platform, process.arch);
  if (!name) {
    return null;
  }
  return name;
}

const launcherSource = fs.readFileSync(path.join(__dirname, "bin", "nestweaver"));

(() => {
  const currentPkg = currentPlatformPackage();
  if (!currentPkg) {
    console.log(
      `  skip - end-to-end resolution tests (${process.platform}-${process.arch} has no published platform package)`,
    );
    return;
  }

  check("the launcher selects and execs the matching platform package, passing args through", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "nestweaver-launcher-test-"));
    try {
      makeFakeNodeModulesTree(tmpDir, [currentPkg]);
      const launcherPath = path.join(tmpDir, "node_modules", "nestweaver", "bin", "nestweaver");
      fs.writeFileSync(launcherPath, launcherSource);
      fs.chmodSync(launcherPath, 0o755);
      const output = execFileSync(process.execPath, [launcherPath, "--version", "extra-arg"], {
        cwd: tmpDir,
        encoding: "utf8",
      });
      assert.match(output, /FAKE_BINARY:\["--version","extra-arg"\]/);
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  check("the launcher propagates the platform binary's exit code", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "nestweaver-launcher-test-"));
    try {
      makeFakeNodeModulesTree(tmpDir, [currentPkg]);
      const launcherPath = path.join(tmpDir, "node_modules", "nestweaver", "bin", "nestweaver");
      fs.writeFileSync(launcherPath, launcherSource);
      fs.chmodSync(launcherPath, 0o755);
      assert.throws(
        () => execFileSync(process.execPath, [launcherPath, "--fail"], { cwd: tmpDir }),
        (err) => err.status === 7,
      );
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  check("a WRONG-platform package alone is correctly NOT selected", () => {
    const wrongPkg = Object.values(PLATFORM_PACKAGES).find((name) => name !== currentPkg);
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "nestweaver-launcher-test-"));
    try {
      makeFakeNodeModulesTree(tmpDir, [wrongPkg]);
      const launcherPath = path.join(tmpDir, "node_modules", "nestweaver", "bin", "nestweaver");
      fs.writeFileSync(launcherPath, launcherSource);
      fs.chmodSync(launcherPath, 0o755);
      let stderr = "";
      try {
        execFileSync(process.execPath, [launcherPath], { cwd: tmpDir, encoding: "utf8" });
        assert.fail("launcher must not succeed when only the wrong platform package is present");
      } catch (err) {
        assert.strictEqual(err.status, 1);
        stderr = String(err.stderr || "");
      }
      assert.match(stderr, /optional dependency/i);
      assert.match(stderr, new RegExp(currentPkg.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  check("no platform package installed at all fails the same clear way", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "nestweaver-launcher-test-"));
    try {
      makeFakeNodeModulesTree(tmpDir, []);
      const launcherPath = path.join(tmpDir, "node_modules", "nestweaver", "bin", "nestweaver");
      fs.writeFileSync(launcherPath, launcherSource);
      fs.chmodSync(launcherPath, 0o755);
      assert.throws(
        () => execFileSync(process.execPath, [launcherPath], { cwd: tmpDir, encoding: "utf8" }),
        (err) => {
          assert.strictEqual(err.status, 1);
          assert.match(String(err.stderr || ""), /optional dependency/i);
          return true;
        },
      );
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });
})();


// ── Release version lockstep ────────────────────────────────────────────
// nw-425/nw-433 replaced a `postinstall` downloader with one
// optionalDependency per platform. That moves a whole class of breakage
// from install time to PUBLISH time: the root package and all four platform
// packages must be published at the SAME version, because the root pins its
// optionalDependencies to an exact version. release-please does that via
// ten `extra-files` entries in release-please-config.json.
//
// The failure mode if one entry is missing is silent and only visible in
// production: the root bumps to N+1 while a platform package stays at N, so
// `npm i nestweaver` either resolves an optionalDependency version that was
// never published (hard failure for that platform only) or, if it happens to
// exist, installs a STALE BINARY under a new version number. Neither shows
// up in CI, because CI never publishes.
//
// These checks derive everything from the filesystem rather than a hardcoded
// list, so ADDING a platform without wiring its config entries fails here
// instead of in someone's install.
const repoRoot = path.join(__dirname, "..");
const rootPkg = JSON.parse(fs.readFileSync(path.join(__dirname, "package.json"), "utf8"));
const platformDirs = fs
  .readdirSync(path.join(__dirname, "platforms"), { withFileTypes: true })
  .filter((e) => e.isDirectory())
  .map((e) => e.name)
  .sort();

check("every platform dir on disk is a known target (no orphan dirs)", () => {
  assert.deepStrictEqual(platformDirs, Object.keys(PLATFORM_PACKAGES).sort());
});

check("every platform package is named and versioned in lockstep with the root", () => {
  for (const dir of platformDirs) {
    const pkg = JSON.parse(
      fs.readFileSync(path.join(__dirname, "platforms", dir, "package.json"), "utf8"),
    );
    assert.strictEqual(pkg.name, `nestweaver-${dir}`, `${dir}: name must be nestweaver-${dir}`);
    assert.strictEqual(
      pkg.version,
      rootPkg.version,
      `${dir}: version ${pkg.version} != root ${rootPkg.version} — a release would publish a stale binary under a new version`,
    );
  }
});

check("root optionalDependencies are exactly the platform packages, pinned to the root version", () => {
  const optional = rootPkg.optionalDependencies || {};
  assert.deepStrictEqual(
    Object.keys(optional).sort(),
    platformDirs.map((d) => `nestweaver-${d}`),
  );
  for (const [name, range] of Object.entries(optional)) {
    assert.strictEqual(
      range,
      rootPkg.version,
      `${name} must be pinned to the exact root version, not a range (${range})`,
    );
  }
});

check("release-please bumps every version this package publishes", () => {
  const cfg = JSON.parse(
    fs.readFileSync(path.join(repoRoot, "release-please-config.json"), "utf8"),
  );
  const extra = cfg.packages["."]["extra-files"] || [];
  const covered = new Set(extra.map((e) => `${e.path}::${e.jsonpath}`));

  const required = ["npm/package.json::$.version"];
  for (const dir of platformDirs) {
    required.push(`npm/platforms/${dir}/package.json::$.version`);
    required.push(`npm/package.json::$.optionalDependencies['nestweaver-${dir}']`);
  }

  const missing = required.filter((r) => !covered.has(r));
  assert.deepStrictEqual(
    missing,
    [],
    `release-please-config.json does not bump: ${missing.join(", ")} — a release would leave these at the previous version`,
  );
});

console.log(`\nbin/nestweaver self-test passed (${passed} assertions)`);
