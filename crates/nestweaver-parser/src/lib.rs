// nestweaver-parser: language-aware source parsing via tree-sitter (code) and comrak (markdown)

pub mod astro;
pub mod cobol;
pub mod entry_points;
pub mod fortran;
pub mod frameworks;
pub mod groovy;
pub mod hcl;
pub mod julia;
pub mod language;
pub mod markdown;
pub mod objc;
pub mod parse;
pub mod pascal;
pub mod powershell;
pub mod registry;
pub mod sql;
pub mod svelte;
pub mod systemverilog;
pub mod vue;
pub mod zig;

pub use entry_points::{detect_entry_point, is_test_file};
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
pub use registry::{LanguageParser, ParserRegistry};
