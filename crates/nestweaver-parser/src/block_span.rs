//! Brace-delimited block spans for the regex-based single-file-component
//! parsers (`astro`, `svelte`, `vue`).
//!
//! These three parsers are line scanners: they match a declaration on one line
//! and then record `end_line: start_line`, so every function they extract has a
//! ZERO-HEIGHT span that excludes its own body. That is the same defect family
//! as `queries/cpp.scm` anchoring on `(function_declarator ..)` — the signature
//! without the body — and it is not cosmetic:
//!
//! - `read-symbols` returns one line where the caller asked for a function.
//! - `nestweaver-resolver`'s `find_enclosing_symbol` cannot place a reference
//!   inside a function whose span is one line, so every call in the body falls
//!   through to a degenerate fallback and is attributed to whatever one-line
//!   symbol happened to precede it.
//!
//! The three parsers had one copy of the defect each, so they get one shared
//! fix rather than three: the property "a symbol's span covers its body" is not
//! a per-language fact.
//!
//! # What this deliberately does not do
//!
//! It is a brace matcher, not a JavaScript parser. It skips braces inside line
//! comments, block comments, and string/template literals (with backslash
//! escapes), because those are the cases that actually occur and that would
//! otherwise silently mis-span a real function. It does NOT understand regex
//! literals, and it requires the opening brace to be on the declaration line —
//! Allman-style `function f()\n{` is not recognised.
//!
//! Every unhandled case returns `None`, which leaves the span exactly as it was
//! before this module existed. A miscount therefore degrades to the old
//! behaviour rather than to a confidently wrong span, which is the only
//! acceptable failure mode for something feeding `read-symbols`.

/// Where the scanner is when it reaches a given character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Code,
    LineComment,
    BlockComment,
    Single,
    Double,
    Template,
}

/// Index (0-based, into `lines`) of the line carrying the brace that closes the
/// block opened on `lines[start]`.
///
/// Returns `None` when `lines[start]` opens no block at all — a genuine
/// one-liner such as `export let count = 0` — and also when the block is never
/// closed within `lines`. Both cases mean "leave the span alone".
pub(crate) fn brace_block_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut opened = false;
    let mut ctx = Ctx::Code;

    for (offset, line) in lines.iter().enumerate().skip(start) {
        // A line comment ends at the newline; the other contexts carry over.
        if ctx == Ctx::LineComment {
            ctx = Ctx::Code;
        }

        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            let next = bytes.get(i + 1).copied();
            match ctx {
                Ctx::LineComment => break,
                Ctx::BlockComment => {
                    if c == '*' && next == Some('/') {
                        ctx = Ctx::Code;
                        i += 1;
                    }
                }
                Ctx::Single | Ctx::Double | Ctx::Template => {
                    if c == '\\' {
                        // Escapes bind to the next character, including a
                        // closing quote. Skip both.
                        i += 1;
                    } else if (ctx == Ctx::Single && c == '\'')
                        || (ctx == Ctx::Double && c == '"')
                        || (ctx == Ctx::Template && c == '`')
                    {
                        ctx = Ctx::Code;
                    }
                }
                Ctx::Code => match c {
                    '/' if next == Some('/') => {
                        ctx = Ctx::LineComment;
                        break;
                    }
                    '/' if next == Some('*') => {
                        ctx = Ctx::BlockComment;
                        i += 1;
                    }
                    '\'' => ctx = Ctx::Single,
                    '"' => ctx = Ctx::Double,
                    '`' => ctx = Ctx::Template,
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => {
                        depth -= 1;
                        if opened && depth <= 0 {
                            return Some(offset);
                        }
                    }
                    _ => {}
                },
            }
            i += 1;
        }

        // The declaration line must open the block. Refusing to scan forward
        // for a brace on a later line is what keeps a non-block declaration
        // (`export let count = 0`) from swallowing the next function.
        if offset == start && !opened {
            return None;
        }
    }

    None
}

/// Convert a 0-based index within a block into the 1-based file line number the
/// parsers record, given the block's `offset` (the 0-based file index of the
/// line *before* the block's first line, which is what all three parsers hold).
pub(crate) fn file_line(offset: u32, idx: usize) -> u32 {
    offset + idx as u32 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end_of(src: &str, start: usize) -> Option<usize> {
        let lines: Vec<&str> = src.lines().collect();
        brace_block_end(&lines, start)
    }

    #[test]
    fn a_function_span_reaches_its_closing_brace() {
        let src = "function handleClick() {\n  count += 1\n  greet('World')\n}\n";
        assert_eq!(end_of(src, 0), Some(3));
    }

    #[test]
    fn a_declaration_that_opens_no_block_is_left_alone() {
        // `export let count = 0` in testdata/svelte/simple.svelte:9 is a
        // GENUINE one-liner. Reporting a span for it would be as wrong as
        // reporting a one-line span for a function.
        assert_eq!(end_of("export let count = 0\nfunction f() {\n}\n", 0), None);
    }

    #[test]
    fn a_brace_inside_a_template_literal_does_not_close_the_block() {
        // testdata/svelte/simple.svelte:5-7 — the body is a template literal
        // with a `${..}` interpolation. Counting its braces as code braces
        // balances by luck here; a literal containing only `}` would not.
        let src = "function greet(name) {\n  return `Hello, ${name}!}`\n}\n";
        assert_eq!(end_of(src, 0), Some(2));
    }

    #[test]
    fn a_brace_inside_a_string_or_comment_does_not_close_the_block() {
        let src = "function f() {\n  const s = \"}\"\n  // }\n  /* } */\n  const c = '}'\n}\n";
        assert_eq!(end_of(src, 0), Some(5));
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let src = "function f() {\n  const s = \"a\\\"}\"\n}\n";
        assert_eq!(end_of(src, 0), Some(2));
    }

    #[test]
    fn nested_blocks_close_at_the_outermost_brace() {
        // testdata/astro/simple.astro:5-10 nests object literals inside an
        // array inside the function body.
        let src = "export function getStaticPaths() {\n  return [\n    { params: { id: '1' } },\n  ]\n}\n";
        assert_eq!(end_of(src, 0), Some(4));
    }

    #[test]
    fn an_unclosed_block_reports_nothing_rather_than_guessing_eof() {
        // Degrading to the pre-existing zero-height span is acceptable;
        // fabricating a span that runs to the end of the file is not.
        assert_eq!(end_of("function f() {\n  broken(\n", 0), None);
    }

    #[test]
    fn a_single_line_block_closes_on_its_own_line() {
        assert_eq!(end_of("function f() { return 1 }\n", 0), Some(0));
    }
}
