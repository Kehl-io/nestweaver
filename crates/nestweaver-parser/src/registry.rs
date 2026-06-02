use crate::parse::{ParseError, ParsedFile};
use std::path::Path;

/// Trait for custom language parsers that can be registered with NestWeaver.
///
/// Implement this trait in your binary to extend NestWeaver with support for
/// additional languages without modifying core library code. Register
/// implementations via `ParserRegistry::register`.
pub trait LanguageParser: Send + Sync {
    /// A short identifier for this language (e.g. `"solidity"`, `"zig"`).
    fn language_id(&self) -> &str;

    /// File extensions handled by this parser (without leading dot, e.g. `"sol"`).
    fn file_extensions(&self) -> &[&str];

    /// Parse a single source file and return its symbols and references.
    fn parse(&self, path: &Path, source: &str) -> Result<ParsedFile, ParseError>;
}

/// A registry of custom language parsers.
///
/// Parsers are matched by file extension in registration order. The first
/// registered parser whose `file_extensions` list contains the file's
/// extension wins.
pub struct ParserRegistry {
    parsers: Vec<Box<dyn LanguageParser>>,
}

impl ParserRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            parsers: Vec::new(),
        }
    }

    /// Register a custom parser. Parsers added first take priority when
    /// multiple parsers claim the same extension.
    pub fn register(&mut self, parser: Box<dyn LanguageParser>) {
        self.parsers.push(parser);
    }

    /// Find the first parser that handles the given path's extension.
    /// Returns `None` if no registered parser matches.
    pub fn find_parser(&self, path: &Path) -> Option<&dyn LanguageParser> {
        let ext = path.extension()?.to_str()?;
        self.parsers
            .iter()
            .find(|p| p.file_extensions().contains(&ext))
            .map(|p| p.as_ref())
    }
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeParser {
        id: &'static str,
        exts: &'static [&'static str],
    }

    impl LanguageParser for FakeParser {
        fn language_id(&self) -> &str {
            self.id
        }
        fn file_extensions(&self) -> &[&str] {
            self.exts
        }
        fn parse(&self, path: &Path, _source: &str) -> Result<ParsedFile, ParseError> {
            Ok(ParsedFile {
                path: path.to_string_lossy().into_owned(),
                symbols: Vec::new(),
                references: Vec::new(),
                type_bindings: Vec::new(),
            })
        }
    }

    #[test]
    fn find_parser_returns_matching_parser() {
        let mut reg = ParserRegistry::new();
        reg.register(Box::new(FakeParser {
            id: "solidity",
            exts: &["sol"],
        }));
        let parser = reg.find_parser(Path::new("contract.sol"));
        assert!(parser.is_some());
        assert_eq!(parser.unwrap().language_id(), "solidity");
    }

    #[test]
    fn find_parser_returns_none_for_unknown_extension() {
        let reg = ParserRegistry::new();
        assert!(reg.find_parser(Path::new("main.zig")).is_none());
    }

    #[test]
    fn first_registered_parser_wins_on_conflict() {
        let mut reg = ParserRegistry::new();
        reg.register(Box::new(FakeParser {
            id: "first",
            exts: &["x"],
        }));
        reg.register(Box::new(FakeParser {
            id: "second",
            exts: &["x"],
        }));
        let parser = reg.find_parser(Path::new("file.x")).unwrap();
        assert_eq!(parser.language_id(), "first");
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = ParserRegistry::default();
        assert!(reg.find_parser(Path::new("any.rs")).is_none());
    }
}
