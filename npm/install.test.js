#!/usr/bin/env node
"use strict";

// Unit tests for the decision logic in install.js.
//
// The installer had no tests at all, and two of its claims were not backed by
// code: it advertised a "runtime version" check that discarded the output it
// was checking, and it hard-failed on an unauthenticated GitHub API call that
// is rate-limited to 60/hr per IP -- so a shared CI egress or corporate NAT
// turned `npm install` into a hard failure with no actionable message.
//
// Run directly (`node install.test.js`); Required CI invokes it.

const assert = require("assert");
const {
  assessReleaseTrust,
  runtimeVersionFailure,
  githubApiHeaders,
  describeReleaseApiFailure,
} = require("./install.js");

let passed = 0;
function check(name, fn) {
  fn();
  passed += 1;
  console.log(`  ok - ${name}`);
}

console.log("install.js self-test");

// ── Release trust ───────────────────────────────────────────────────────
check("an immutable release for the exact tag is clean", () => {
  const trust = assessReleaseTrust({ immutable: true, tag_name: "v9.0.6" }, "v9.0.6", {});
  assert.strictEqual(trust.failure, null);
  assert.strictEqual(trust.warning, null);
});

check("a release for a different tag is REFUSED", () => {
  const trust = assessReleaseTrust({ immutable: true, tag_name: "v9.0.5" }, "v9.0.6", {});
  assert.match(trust.failure, /does not match/);
});

// EVERY release published before immutable releases were enabled reports
// `immutable: false` -- verified against the live API for v9.0.0, v9.0.4 and
// v9.0.5. Refusing them outright would make `npm install` fail for every
// version currently on the registry, which is a self-inflicted outage rather
// than a security control. The checksum is still verified; what is missing is
// tamper-EVIDENCE, and that is disclosed rather than claimed or silently
// dropped.
check("a non-immutable release is disclosed, not refused", () => {
  const trust = assessReleaseTrust({ immutable: false, tag_name: "v9.0.5" }, "v9.0.5", {});
  assert.strictEqual(trust.failure, null, "must not break install for published versions");
  assert.match(trust.warning, /not immutable/i);
  assert.match(trust.warning, /NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE/);
});

check("a release missing the immutable field is disclosed the same way", () => {
  const trust = assessReleaseTrust({ tag_name: "v9.0.5" }, "v9.0.5", {});
  assert.strictEqual(trust.failure, null);
  assert.match(trust.warning, /not immutable/i);
});

check("strict mode turns the disclosure into a refusal", () => {
  const trust = assessReleaseTrust(
    { immutable: false, tag_name: "v9.0.5" },
    "v9.0.5",
    { NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE: "1" },
  );
  assert.match(trust.failure, /not immutable/i);
});

check("strict mode does not reject an immutable release", () => {
  const trust = assessReleaseTrust(
    { immutable: true, tag_name: "v9.0.6" },
    "v9.0.6",
    { NESTWEAVER_REQUIRE_IMMUTABLE_RELEASE: "1" },
  );
  assert.strictEqual(trust.failure, null);
});

// ── Runtime version ─────────────────────────────────────────────────────
// The installed binary must be the version the package claims. Running
// `--version` only proves the file executes; it does not prove identity.
check("a matching runtime version passes", () => {
  assert.strictEqual(runtimeVersionFailure("nestweaver 9.0.6\n", "9.0.6"), null);
});

check("a mismatched runtime version is caught", () => {
  const failure = runtimeVersionFailure("nestweaver 9.0.5\n", "9.0.6");
  assert.match(failure, /9\.0\.5/);
  assert.match(failure, /9\.0\.6/);
});

check("a version that is only a prefix of another does not pass", () => {
  assert.notStrictEqual(runtimeVersionFailure("nestweaver 9.0.60\n", "9.0.6"), null);
});

check("empty or unparseable --version output is caught", () => {
  assert.notStrictEqual(runtimeVersionFailure("", "9.0.6"), null);
  assert.notStrictEqual(runtimeVersionFailure("garbage\n", "9.0.6"), null);
});

// ── API authentication ──────────────────────────────────────────────────
// Unauthenticated api.github.com allows 60 requests/hr/IP. A token raises it
// to 5,000 and is already present in virtually every CI environment.
check("no token yields no Authorization header", () => {
  const headers = githubApiHeaders({});
  assert.ok(!headers.some((h) => h.startsWith("Authorization")));
});

check("GITHUB_TOKEN is sent as a bearer credential", () => {
  assert.ok(
    githubApiHeaders({ GITHUB_TOKEN: "abc" }).includes("Authorization: Bearer abc"),
  );
});

check("GH_TOKEN is honoured as a fallback", () => {
  assert.ok(
    githubApiHeaders({ GH_TOKEN: "xyz" }).includes("Authorization: Bearer xyz"),
  );
});

check("GITHUB_TOKEN wins over GH_TOKEN", () => {
  const headers = githubApiHeaders({ GITHUB_TOKEN: "a", GH_TOKEN: "b" });
  assert.ok(headers.includes("Authorization: Bearer a"));
  assert.ok(!headers.includes("Authorization: Bearer b"));
});

check("a blank token is treated as absent", () => {
  assert.ok(
    !githubApiHeaders({ GITHUB_TOKEN: "   " }).some((h) =>
      h.startsWith("Authorization"),
    ),
  );
});

// ── Failure diagnosis ───────────────────────────────────────────────────
// Failing closed is right; failing closed without saying how to fix it is not.
check("a rate-limited response names the token variable", () => {
  const message = describeReleaseApiFailure(
    { status: 403, body: "API rate limit exceeded for 1.2.3.4" },
    {},
  );
  assert.match(message, /rate limit/i);
  assert.match(message, /GITHUB_TOKEN/);
});

check("a rate-limit message does not tell an authenticated caller to set a token", () => {
  const message = describeReleaseApiFailure(
    { status: 403, body: "API rate limit exceeded" },
    { GITHUB_TOKEN: "abc" },
  );
  assert.match(message, /rate limit/i);
  assert.ok(
    !/set .*GITHUB_TOKEN/i.test(message),
    `already-authenticated caller should not be told to set a token: ${message}`,
  );
});

check("a 404 is reported as a missing release, not a rate limit", () => {
  const message = describeReleaseApiFailure({ status: 404, body: "Not Found" }, {});
  assert.match(message, /not found/i);
  assert.ok(!/rate limit/i.test(message));
});

check("an unclassified failure still reports its cause", () => {
  assert.match(
    describeReleaseApiFailure({ status: 0, body: "connection reset" }, {}),
    /connection reset/,
  );
});

console.log(`\ninstall.js self-test passed (${passed} assertions)`);
