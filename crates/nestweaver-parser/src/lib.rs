// nestweaver-parser: language-aware source parsing via tree-sitter (code) and comrak (markdown)

pub mod cobol;
pub mod entry_points;
pub mod frameworks;
pub mod language;
pub mod markdown;
pub mod parse;

pub use entry_points::detect_entry_point;
pub use frameworks::detect_frameworks;
pub use language::{detect_language, is_markdown};
pub use markdown::{
    MarkdownParseError, ParsedNote, RawHeading, RawSection, RawTag, RawWikilink, TagSource,
    parse_markdown,
};
pub use parse::{
    ParseError, ParseResult, ParsedFile, RawReference, RawSymbol, ReferenceKind, SkippedFile,
    parse_batch, parse_source,
};
