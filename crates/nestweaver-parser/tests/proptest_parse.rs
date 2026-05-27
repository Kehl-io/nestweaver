use nestweaver_parser::parse_source;
use proptest::prelude::*;
use std::path::Path;

// ── strategies ────────────────────────────────────────────────────────────

/// Generate a valid JS identifier: starts with a letter, followed by alphanumerics.
fn arb_identifier() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-zA-Z][a-zA-Z0-9]{0,15}").expect("valid regex for identifier")
}

/// Generate JS-like source with function definitions and calls.
fn arb_js_source() -> impl Strategy<Value = String> {
    prop::collection::vec((arb_identifier(), arb_identifier()), 1..=5).prop_map(|pairs| {
        let mut src = String::new();
        for (fn_name, param) in &pairs {
            src.push_str(&format!(
                "function {}({}) {{\n  return {};\n}}\n\n",
                fn_name, param, param
            ));
        }
        // Add some calls
        for (fn_name, param) in &pairs {
            src.push_str(&format!("{}({});\n", fn_name, param));
        }
        src
    })
}

/// Generate Python-like source with def blocks and calls.
fn arb_python_source() -> impl Strategy<Value = String> {
    prop::collection::vec((arb_identifier(), arb_identifier()), 1..=5).prop_map(|pairs| {
        let mut src = String::new();
        for (fn_name, param) in &pairs {
            src.push_str(&format!(
                "def {}({}):\n    return {}\n\n",
                fn_name, param, param
            ));
        }
        // Add some calls
        for (fn_name, param) in &pairs {
            src.push_str(&format!("{}({})\n", fn_name, param));
        }
        src
    })
}

// ── property tests ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Feeding arbitrary printable strings as JS must never panic.
    /// The parser may return Ok or Err, but it must not crash.
    #[test]
    fn parser_never_panics_on_arbitrary_input(source in "\\PC{0,2000}") {
        // Just call it — we only care that it doesn't panic.
        let _ = parse_source(Path::new("test.js"), &source);
    }

    /// Generated JS-like source (valid function defs + calls) must parse successfully.
    #[test]
    fn parser_handles_generated_js(source in arb_js_source()) {
        let result = parse_source(Path::new("test.js"), &source);
        prop_assert!(result.is_ok(), "parse_source failed on generated JS: {:?}\nsource:\n{}", result.err(), source);
    }

    /// Generated Python-like source (valid def blocks + calls) must parse successfully.
    #[test]
    fn parser_handles_generated_python(source in arb_python_source()) {
        let result = parse_source(Path::new("test.py"), &source);
        prop_assert!(result.is_ok(), "parse_source failed on generated Python: {:?}\nsource:\n{}", result.err(), source);
    }

    /// All extracted symbols must have non-empty names.
    #[test]
    fn all_symbols_have_names(source in arb_js_source()) {
        let parsed = parse_source(Path::new("test.js"), &source).unwrap();
        for sym in &parsed.symbols {
            prop_assert!(!sym.name.is_empty(), "symbol has empty name: {:?}", sym);
        }
    }

    /// All extracted references must have non-empty names.
    #[test]
    fn all_references_have_names(source in arb_js_source()) {
        let parsed = parse_source(Path::new("test.js"), &source).unwrap();
        for reference in &parsed.references {
            prop_assert!(!reference.name.is_empty(), "reference has empty name: {:?}", reference);
        }
    }

    /// Symbol start_line must not exceed the number of lines in the source.
    #[test]
    fn symbol_lines_within_bounds(source in arb_js_source()) {
        let line_count = source.lines().count() as u32;
        let parsed = parse_source(Path::new("test.js"), &source).unwrap();
        for sym in &parsed.symbols {
            prop_assert!(
                sym.start_line <= line_count,
                "symbol '{}' has start_line {} but source only has {} lines",
                sym.name, sym.start_line, line_count
            );
        }
    }

    /// Parsing the same source twice must yield identical results.
    #[test]
    fn parsing_is_deterministic(source in arb_js_source()) {
        let path = Path::new("test.js");
        let result1 = parse_source(path, &source).unwrap();
        let result2 = parse_source(path, &source).unwrap();

        prop_assert_eq!(result1.symbols.len(), result2.symbols.len(), "symbol count differs");
        prop_assert_eq!(result1.references.len(), result2.references.len(), "reference count differs");

        for (s1, s2) in result1.symbols.iter().zip(result2.symbols.iter()) {
            prop_assert_eq!(&s1.name, &s2.name, "symbol name differs");
            prop_assert_eq!(s1.start_line, s2.start_line, "symbol start_line differs");
            prop_assert_eq!(&s1.content_hash, &s2.content_hash, "symbol content_hash differs");
            prop_assert_eq!(&s1.signature, &s2.signature, "symbol signature differs");
        }

        for (r1, r2) in result1.references.iter().zip(result2.references.iter()) {
            prop_assert_eq!(&r1.name, &r2.name, "reference name differs");
            prop_assert_eq!(r1.start_line, r2.start_line, "reference start_line differs");
            prop_assert_eq!(&r1.context, &r2.context, "reference context differs");
        }
    }
}
