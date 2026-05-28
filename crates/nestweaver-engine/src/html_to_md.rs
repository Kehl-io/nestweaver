//! HTML-to-markdown preprocessing for wiki content ingestion.
//!
//! Wiki PRDs fetched via MCP (e.g. from Confluence) arrive as HTML storage
//! format. Feeding raw HTML to comrak produces a single Note with one Section
//! and zero Heading nodes — killing heading-level search and PPR navigation.
//!
//! This module detects HTML content and converts it to markdown using `html2md`
//! so the downstream markdown parser can extract proper structure.

/// Convert `content` from HTML to markdown if it looks like HTML, otherwise
/// return it unchanged.
///
/// The heuristic is intentionally conservative: we only convert when the
/// trimmed content starts with `<` or contains common block-level HTML tags.
/// Plain markdown that happens to contain an inline `<code>` or `<em>` tag
/// won't be converted because those don't appear at the start and aren't
/// among the block-level markers we check.
pub fn maybe_convert_html_to_markdown(content: &str) -> String {
    let trimmed = content.trim();
    if looks_like_html(trimmed) {
        html2md::parse_html(content)
    } else {
        content.to_string()
    }
}

/// Returns `true` when `trimmed` (already whitespace-stripped) appears to be
/// HTML rather than markdown.
fn looks_like_html(trimmed: &str) -> bool {
    if trimmed.starts_with('<') {
        return true;
    }
    // Check for common block-level HTML tags anywhere in the content.
    const MARKERS: &[&str] = &["<h1", "<h2", "<h3", "<p>", "<div", "<table"];
    MARKERS.iter().any(|m| trimmed.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_markdown_passes_through() {
        let md = "# Title\n\nSome paragraph text.\n\n## Section\n\nMore text.\n";
        let result = maybe_convert_html_to_markdown(md);
        assert_eq!(result, md);
    }

    #[test]
    fn html_with_headings_converts_to_markdown() {
        let html = "<h1>Title</h1><p>Intro paragraph.</p><h2>Details</h2><p>Body text.</p>";
        let result = maybe_convert_html_to_markdown(html);

        // html2md uses setext headings for H1/H2 (underline with === / ---).
        // Verify the heading text and paragraph text survive conversion.
        assert!(result.contains("Title"), "expected 'Title' in:\n{result}");
        assert!(
            result.contains("Details"),
            "expected 'Details' in:\n{result}"
        );
        assert!(
            result.contains("Intro paragraph."),
            "expected paragraph text in:\n{result}"
        );

        // The real test: comrak parses the converted markdown into headings.
        let parsed = nestweaver_parser::parse_markdown("wiki/test", &result).unwrap();
        let heading_texts: Vec<&str> = parsed.headings.iter().map(|h| h.text.as_str()).collect();
        assert!(
            heading_texts.contains(&"Title"),
            "comrak missing 'Title' heading; got: {heading_texts:?}"
        );
        assert!(
            heading_texts.contains(&"Details"),
            "comrak missing 'Details' heading; got: {heading_texts:?}"
        );
    }

    #[test]
    fn html_with_table_converts() {
        let html = "<table><tr><th>Name</th></tr><tr><td>Alice</td></tr></table>";
        let result = maybe_convert_html_to_markdown(html);
        assert!(
            result.contains("Alice"),
            "expected table content in:\n{result}"
        );
    }

    #[test]
    fn whitespace_prefixed_html_detected() {
        let html = "  \n  <h1>Title</h1><p>Body</p>";
        let result = maybe_convert_html_to_markdown(html);
        // Verify conversion happened (heading text present, not raw HTML).
        assert!(
            !result.contains("<h1>"),
            "raw HTML tags should be converted:\n{result}"
        );
        assert!(
            result.contains("Title"),
            "expected heading text in:\n{result}"
        );
    }

    #[test]
    fn empty_string_passes_through() {
        let result = maybe_convert_html_to_markdown("");
        assert_eq!(result, "");
    }

    /// End-to-end test: Confluence-like HTML produces Heading nodes after
    /// html2md conversion + comrak parsing.
    #[test]
    fn confluence_like_html_produces_headings() {
        let html = r#"<h1>Product Requirements</h1>
<p>This document describes the feature.</p>
<h2>Background</h2>
<p>Some background context here.</p>
<h2>Requirements</h2>
<h3>Functional</h3>
<p>The system shall do X.</p>
<h3>Non-Functional</h3>
<p>The system shall handle Y.</p>"#;

        let result = maybe_convert_html_to_markdown(html);

        // Verify raw HTML is gone.
        assert!(
            !result.contains("<h1>"),
            "raw HTML should be converted:\n{result}"
        );

        // Verify that comrak parses the converted markdown into proper
        // Heading nodes (the whole point of this conversion).
        let parsed = nestweaver_parser::parse_markdown("wiki/test", &result).unwrap();
        let heading_texts: Vec<&str> = parsed.headings.iter().map(|h| h.text.as_str()).collect();
        assert!(
            heading_texts.contains(&"Product Requirements"),
            "comrak did not produce H1 heading; got: {heading_texts:?}"
        );
        assert!(
            heading_texts.contains(&"Background"),
            "comrak did not produce 'Background' heading; got: {heading_texts:?}"
        );
        assert!(
            heading_texts.contains(&"Requirements"),
            "comrak did not produce 'Requirements' heading; got: {heading_texts:?}"
        );
        assert!(
            heading_texts.contains(&"Functional"),
            "comrak did not produce 'Functional' heading; got: {heading_texts:?}"
        );
        assert!(
            heading_texts.contains(&"Non-Functional"),
            "comrak did not produce 'Non-Functional' heading; got: {heading_texts:?}"
        );
        assert!(
            parsed.headings.len() >= 5,
            "expected at least 5 headings, got {}",
            parsed.headings.len()
        );

        // Verify sections were also generated (one per heading).
        assert!(
            parsed.sections.len() >= 5,
            "expected at least 5 sections, got {}",
            parsed.sections.len()
        );
    }

    /// Verifies that raw HTML passed directly to comrak (without conversion)
    /// would produce poor results — confirming the need for this module.
    #[test]
    fn raw_html_without_conversion_has_no_headings() {
        let html = "<h1>Title</h1><p>Body text.</p><h2>Section</h2><p>More text.</p>";
        let parsed = nestweaver_parser::parse_markdown("wiki/test", html).unwrap();
        // comrak treats raw HTML as inline content, producing zero headings.
        assert_eq!(
            parsed.headings.len(),
            0,
            "raw HTML should produce 0 headings in comrak, got: {:?}",
            parsed.headings.iter().map(|h| &h.text).collect::<Vec<_>>()
        );
    }
}
