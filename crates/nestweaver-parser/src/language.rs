use nestweaver_schema::Language;
use std::path::Path;

/// Detect the programming language from a file's extension.
/// Returns `None` for unsupported or missing extensions.
///
/// Markdown is NOT a code language and is detected separately via
/// [`is_markdown`] — keeping `Language` strictly code-typed avoids
/// breaking the exhaustive matches in the code resolver and confidence
/// scoring.
pub fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "js" => Some(Language::JavaScript),
        "ts" | "tsx" => Some(Language::TypeScript),
        "java" => Some(Language::Java),
        "go" => Some(Language::Go),
        "py" => Some(Language::Python),
        "cpp" | "cc" | "cxx" | "hpp" => Some(Language::Cpp),
        "rs" => Some(Language::Rust),
        "kt" | "kts" => Some(Language::Kotlin),
        "cs" => Some(Language::CSharp),
        "php" => Some(Language::Php),
        "rb" | "rake" => Some(Language::Ruby),
        "swift" => Some(Language::Swift),
        "c" | "h" => Some(Language::C),
        "dart" => Some(Language::Dart),
        "cbl" | "cob" | "cpy" => Some(Language::Cobol),
        _ => None,
    }
}

/// True if `path` looks like a Markdown file (`.md` or `.markdown`).
pub fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detect_language_js() {
        assert_eq!(
            detect_language(Path::new("foo.js")),
            Some(Language::JavaScript)
        );
    }

    #[test]
    fn detect_language_ts() {
        assert_eq!(
            detect_language(Path::new("foo.ts")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn detect_language_tsx() {
        assert_eq!(
            detect_language(Path::new("foo.tsx")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn detect_language_java() {
        assert_eq!(detect_language(Path::new("Foo.java")), Some(Language::Java));
    }

    #[test]
    fn detect_language_go() {
        assert_eq!(detect_language(Path::new("foo.go")), Some(Language::Go));
    }

    #[test]
    fn detect_language_python() {
        assert_eq!(detect_language(Path::new("foo.py")), Some(Language::Python));
    }

    #[test]
    fn detect_language_unsupported() {
        assert_eq!(detect_language(Path::new("foo.zig")), None);
    }

    #[test]
    fn detect_language_no_extension() {
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }

    #[test]
    fn detect_language_kotlin() {
        assert_eq!(detect_language(Path::new("Foo.kt")), Some(Language::Kotlin));
    }

    #[test]
    fn detect_language_kotlin_script() {
        assert_eq!(
            detect_language(Path::new("build.kts")),
            Some(Language::Kotlin)
        );
    }

    #[test]
    fn detect_language_csharp() {
        assert_eq!(detect_language(Path::new("Foo.cs")), Some(Language::CSharp));
    }

    #[test]
    fn detect_language_php() {
        assert_eq!(detect_language(Path::new("index.php")), Some(Language::Php));
    }

    #[test]
    fn detect_language_ruby() {
        assert_eq!(detect_language(Path::new("app.rb")), Some(Language::Ruby));
    }

    #[test]
    fn detect_language_ruby_rake() {
        assert_eq!(
            detect_language(Path::new("task.rake")),
            Some(Language::Ruby)
        );
    }

    #[test]
    fn detect_language_swift() {
        assert_eq!(
            detect_language(Path::new("main.swift")),
            Some(Language::Swift)
        );
    }

    #[test]
    fn detect_language_c() {
        assert_eq!(detect_language(Path::new("main.c")), Some(Language::C));
    }

    #[test]
    fn detect_language_c_header() {
        assert_eq!(detect_language(Path::new("header.h")), Some(Language::C));
    }

    #[test]
    fn detect_language_dart() {
        assert_eq!(
            detect_language(Path::new("main.dart")),
            Some(Language::Dart)
        );
    }

    #[test]
    fn detect_language_cobol() {
        assert_eq!(
            detect_language(Path::new("prog.cbl")),
            Some(Language::Cobol)
        );
    }

    #[test]
    fn detect_language_cobol_cob() {
        assert_eq!(
            detect_language(Path::new("prog.cob")),
            Some(Language::Cobol)
        );
    }

    #[test]
    fn detect_language_cobol_copybook() {
        assert_eq!(
            detect_language(Path::new("copy.cpy")),
            Some(Language::Cobol)
        );
    }

    #[test]
    fn detect_language_hpp_still_cpp() {
        assert_eq!(detect_language(Path::new("foo.hpp")), Some(Language::Cpp));
    }
}
