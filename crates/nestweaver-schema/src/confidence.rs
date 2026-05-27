use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    JavaScript,
    TypeScript,
    Java,
    Go,
    Python,
    Cpp,
    Rust,
    Kotlin,
    CSharp,
    Php,
    Ruby,
    Swift,
    C,
    Dart,
    Cobol,
    Lua,
    Bash,
    Scala,
    Elixir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MatchType {
    SameFileExact,
    ImportResolved,
    ReExportResolved,
    SamePackageFallback,
    Unresolved,
}

/// Compute a confidence score in [0.0, 1.0] for an edge match.
///
/// Scoring table:
/// | Match              | Base | Java/Go  | Python | JS/TS  |
/// |--------------------|------|----------|--------|--------|
/// | SameFileExact      | 0.95 |    —     |   —    |   —    |
/// | ImportResolved     | 0.90 |  +0.05   | -0.10  |   —    |
/// | ReExportResolved   | 0.75 |    —     |   —    | -0.05  |
/// | SamePackageFallback| 0.50 |    —     | -0.15  |   —    |
/// | Unresolved         | 0.00 |    —     |   —    |   —    |
pub fn confidence_score(match_type: MatchType, language: Language) -> f32 {
    let base: f32 = match match_type {
        MatchType::SameFileExact => 0.95,
        MatchType::ImportResolved => 0.90,
        MatchType::ReExportResolved => 0.75,
        MatchType::SamePackageFallback => 0.50,
        MatchType::Unresolved => 0.00,
    };

    let modifier: f32 = match (match_type, language) {
        (
            MatchType::ImportResolved,
            Language::Java
            | Language::Go
            | Language::Cpp
            | Language::Rust
            | Language::Kotlin
            | Language::CSharp,
        ) => 0.05,
        (MatchType::ImportResolved, Language::Dart | Language::Swift) => 0.03,
        (MatchType::ImportResolved, Language::Scala) => 0.05,
        (MatchType::ImportResolved, Language::Elixir) => -0.05,
        (MatchType::ImportResolved, Language::Lua) => -0.10,
        (MatchType::ImportResolved, Language::Bash) => -0.15,
        (MatchType::ImportResolved, Language::Python | Language::Ruby) => -0.10,
        (MatchType::ImportResolved, Language::Php) => -0.05,
        (MatchType::ImportResolved, Language::Cobol) => -0.15,
        (MatchType::ReExportResolved, Language::JavaScript | Language::TypeScript) => -0.05,
        (MatchType::SamePackageFallback, Language::Python) => -0.15,
        (MatchType::SamePackageFallback, Language::Ruby) => -0.15,
        _ => 0.0,
    };

    (base + modifier).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_exact_is_0_95() {
        let score = confidence_score(MatchType::SameFileExact, Language::Python);
        assert!((score - 0.95).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn unresolved_is_zero() {
        for lang in [
            Language::JavaScript,
            Language::TypeScript,
            Language::Java,
            Language::Go,
            Language::Python,
            Language::Cpp,
            Language::Rust,
            Language::Kotlin,
            Language::CSharp,
            Language::Php,
            Language::Ruby,
            Language::Swift,
            Language::C,
            Language::Dart,
            Language::Cobol,
            Language::Lua,
            Language::Bash,
            Language::Scala,
            Language::Elixir,
        ] {
            let score = confidence_score(MatchType::Unresolved, lang);
            assert!((score - 0.0).abs() < f32::EPSILON, "got: {score}");
        }
    }

    #[test]
    fn python_import_lower_than_java() {
        let java = confidence_score(MatchType::ImportResolved, Language::Java);
        let python = confidence_score(MatchType::ImportResolved, Language::Python);
        assert!(
            python < java,
            "python ({python}) should be less than java ({java})"
        );
    }

    #[test]
    fn java_import_resolved_is_0_95() {
        let score = confidence_score(MatchType::ImportResolved, Language::Java);
        assert!((score - 0.95).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn python_import_resolved_is_0_80() {
        let score = confidence_score(MatchType::ImportResolved, Language::Python);
        assert!((score - 0.80).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn reexport_js_modifier() {
        let ts = confidence_score(MatchType::ReExportResolved, Language::TypeScript);
        let java = confidence_score(MatchType::ReExportResolved, Language::Java);
        assert!((ts - 0.70).abs() < f32::EPSILON, "got: {ts}");
        assert!((java - 0.75).abs() < f32::EPSILON, "got: {java}");
    }

    #[test]
    fn same_package_fallback_python() {
        let score = confidence_score(MatchType::SamePackageFallback, Language::Python);
        assert!((score - 0.35).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn all_scores_clamped_to_0_1() {
        let langs = [
            Language::JavaScript,
            Language::TypeScript,
            Language::Java,
            Language::Go,
            Language::Python,
            Language::Cpp,
            Language::Rust,
            Language::Kotlin,
            Language::CSharp,
            Language::Php,
            Language::Ruby,
            Language::Swift,
            Language::C,
            Language::Dart,
            Language::Cobol,
            Language::Lua,
            Language::Bash,
            Language::Scala,
            Language::Elixir,
        ];
        let matches = [
            MatchType::SameFileExact,
            MatchType::ImportResolved,
            MatchType::ReExportResolved,
            MatchType::SamePackageFallback,
            MatchType::Unresolved,
        ];
        for m in matches {
            for l in langs {
                let s = confidence_score(m, l);
                assert!(
                    (0.0..=1.0).contains(&s),
                    "score out of range for {m:?}/{l:?}: {s}"
                );
            }
        }
    }

    #[test]
    fn kotlin_import_resolved_is_0_95() {
        let score = confidence_score(MatchType::ImportResolved, Language::Kotlin);
        assert!((score - 0.95).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn csharp_import_resolved_is_0_95() {
        let score = confidence_score(MatchType::ImportResolved, Language::CSharp);
        assert!((score - 0.95).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn dart_import_resolved_is_0_93() {
        let score = confidence_score(MatchType::ImportResolved, Language::Dart);
        assert!((score - 0.93).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn swift_import_resolved_is_0_93() {
        let score = confidence_score(MatchType::ImportResolved, Language::Swift);
        assert!((score - 0.93).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn c_import_resolved_is_0_90() {
        let score = confidence_score(MatchType::ImportResolved, Language::C);
        assert!((score - 0.90).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn php_import_resolved_is_0_85() {
        let score = confidence_score(MatchType::ImportResolved, Language::Php);
        assert!((score - 0.85).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn ruby_import_resolved_is_0_80() {
        let score = confidence_score(MatchType::ImportResolved, Language::Ruby);
        assert!((score - 0.80).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn cobol_import_resolved_is_0_75() {
        let score = confidence_score(MatchType::ImportResolved, Language::Cobol);
        assert!((score - 0.75).abs() < f32::EPSILON, "got: {score}");
    }

    #[test]
    fn all_new_languages_scores_clamped_to_0_1() {
        let new_langs = [
            Language::Kotlin,
            Language::CSharp,
            Language::Php,
            Language::Ruby,
            Language::Swift,
            Language::C,
            Language::Dart,
            Language::Cobol,
            Language::Lua,
            Language::Bash,
            Language::Scala,
            Language::Elixir,
        ];
        let matches = [
            MatchType::SameFileExact,
            MatchType::ImportResolved,
            MatchType::ReExportResolved,
            MatchType::SamePackageFallback,
            MatchType::Unresolved,
        ];
        for m in matches {
            for l in new_langs {
                let s = confidence_score(m, l);
                assert!(
                    (0.0..=1.0).contains(&s),
                    "score out of range for {m:?}/{l:?}: {s}"
                );
            }
        }
    }
}
