// Delimiter rules match the markdown parser's split_frontmatter: a leading
// block opens on a line that is exactly `---`, and closes on the next line
// that is exactly `---` after trailing whitespace. An opening delimiter with
// no closing line is not frontmatter, so the note stays intact. This does not
// parse YAML and does not remove headings.

export interface StrippedFrontmatter {
  body: string;
  /** File lines removed from the start. Body line N is file line N + lineOffset. */
  lineOffset: number;
}

function lineCount(text: string): number {
  if (text.length === 0) return 0;
  let count = 0;
  let index = 0;
  while (index < text.length) {
    const newline = text.indexOf("\n", index);
    if (newline === -1) return count + 1;
    count += 1;
    index = newline + 1;
  }
  return count;
}

export function stripLeadingFrontmatter(source: string): StrippedFrontmatter {
  const bom = source.charCodeAt(0) === 0xfeff;
  const text = bom ? source.slice(1) : source;
  const opened = text.startsWith("---\r\n") || text.startsWith("---\n");
  if (!opened) return { body: source, lineOffset: 0 };

  const rest = text.slice(text.startsWith("---\r\n") ? 5 : 4);
  let offset = 0;
  while (offset <= rest.length) {
    const newline = rest.indexOf("\n", offset);
    const lineEnd = newline === -1 ? rest.length : newline;
    const line = rest.slice(offset, lineEnd).replace(/\r$/, "");
    if (line.trimEnd() === "---") {
      const body = rest.slice(newline === -1 ? lineEnd : newline + 1);
      return { body, lineOffset: lineCount(source) - lineCount(body) };
    }
    if (newline === -1) break;
    offset = newline + 1;
  }

  // Opening delimiter without a closing line. Leave the note unchanged.
  return { body: source, lineOffset: 0 };
}
