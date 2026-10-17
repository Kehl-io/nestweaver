import assert from "node:assert/strict";
import test from "node:test";
import { stripLeadingFrontmatter } from "./stripFrontmatter.ts";

const note = `---
title: From Frontmatter
type: design
---

# H1 Title

Body with [[Savr Overview]] and a rule below.

---

\`\`\`yaml
---
still: code
---
\`\`\`
`;

test("note preview strips a leading frontmatter block and keeps markdown", () => {
  const stripped = stripLeadingFrontmatter(note);

  assert.equal(stripped.lineOffset, 4);
  assert.equal(stripped.body.startsWith("\n# H1 Title\n"), true);
  assert.equal(stripped.body.includes("title: From Frontmatter"), false);
  assert.equal(stripped.body.includes("[[Savr Overview]]"), true);
  assert.match(stripped.body, /^# H1 Title/m);
  assert.match(stripped.body, /```yaml\n---\nstill: code\n---\n```/);

  const headingLine = stripped.body.split("\n").findIndex((line) => line === "# H1 Title") + 1;
  assert.equal(headingLine + stripped.lineOffset, 6);
});

test("malformed delimiters are explicit and do not swallow the note", () => {
  const unclosed = "---\ntitle: forever\n\n# Body Heading\nbody text\n";
  assert.deepEqual(stripLeadingFrontmatter(unclosed), { body: unclosed, lineOffset: 0 });

  const inlineCloser = "---\ntitle: hello --- world\n---\n# Kept\n";
  const closed = stripLeadingFrontmatter(inlineCloser);
  assert.equal(closed.body, "# Kept\n");
  assert.equal(closed.lineOffset, 3);

  const notADelimiter = "----\ntitle: x\n----\n# Heading\n";
  assert.equal(stripLeadingFrontmatter(notADelimiter).body, notADelimiter);

  const brokenYaml = "---\ntitle: \"unclosed\nbroken: [\n---\n\n# Still A Note\n";
  assert.equal(stripLeadingFrontmatter(brokenYaml).body, "\n# Still A Note\n");
});
