// nestweaver-parser: language-aware source parsing via tree-sitter (code) and comrak (markdown)

pub mod astro;
pub mod canvas;
pub mod cobol;
pub mod dataview;
pub mod entry_points;
pub mod frameworks;
pub mod language;
pub mod markdown;
pub mod mermaid;
pub mod parse;
pub mod registry;
pub mod svelte;
pub mod vue;

pub use canvas::{CanvasEdge, CanvasFile, CanvasNode, parse_canvas};
pub use dataview::{DataviewQuery, parse_dataview_query};
pub use entry_points::{detect_entry_point, is_test_file};
pub use frameworks::detect_frameworks;
pub use language::{detect_language, is_markdown};
pub use markdown::{
    MarkdownParseError, ParsedNote, RawHeading, RawSection, RawTag, RawWikilink, TagSource,
    parse_markdown,
};
pub use mermaid::{MermaidDiagram, MermaidEdge, MermaidNode, parse_mermaid};
pub use parse::{
    AstBindingKind, AstTypeBinding, ParseError, ParseResult, ParsedFile, RawReference, RawSymbol,
    ReferenceKind, SkippedFile, parse_batch, parse_source,
};
pub use registry::{LanguageParser, ParserRegistry};
