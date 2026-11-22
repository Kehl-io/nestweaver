use std::path::Path;

use nestweaver_parser::{SkipReasonCode, parse_batch, parse_source};
use nestweaver_schema::{SymbolKind, Visibility};

fn parse_fixture(relative: &str) -> nestweaver_parser::ParsedFile {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(relative);
    let source = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| panic!("read {}: {error}", fixture.display()));
    parse_source(Path::new(relative), &source)
        .unwrap_or_else(|error| panic!("parse {relative}: {error}"))
}

#[test]
fn pascal_interface_declarations_have_own_spans_and_signatures() {
    let parsed = parse_fixture("pascal/declarations.pas");
    for (name, declaration_line, signature) in [
        ("First", 5, "procedure First;"),
        ("Second", 6, "function Second: Integer;"),
    ] {
        let occurrences: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == name)
            .collect();
        assert_eq!(
            occurrences.len(),
            2,
            "declaration and implementation for {name}: {occurrences:?}"
        );
        assert!(occurrences.iter().any(|symbol| {
            symbol.kind == SymbolKind::Function
                && symbol.start_line == declaration_line
                && symbol.end_line == declaration_line
                && symbol.signature == signature
        }));
        assert!(
            occurrences
                .iter()
                .any(|symbol| symbol.start_line > declaration_line)
        );
    }
}

#[test]
fn elixir_declaration_heads_include_zero_arity_guards_keywords_and_delegates() {
    let parsed = parse_fixture("elixir/forms.ex");
    for name in [
        "zero",
        "keyword",
        "guarded",
        "private_guarded",
        "macro_zero",
        "guarded_macro",
        "private_macro",
        "positive",
        "nonzero",
        "delegated",
        "multi",
    ] {
        assert!(
            parsed.symbols.iter().any(|symbol| symbol.name == name),
            "missing Elixir definition {name}; found {:?}",
            parsed
                .symbols
                .iter()
                .map(|symbol| &symbol.name)
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(
        parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "multi")
            .count(),
        2,
        "multi-clause definitions remain distinct"
    );
    for name in ["private_guarded", "private_macro", "nonzero"] {
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| { symbol.name == name && symbol.visibility == Visibility::Private }),
            "private Elixir definition {name} must remain private"
        );
    }
}

#[test]
fn systemverilog_module_functions_and_tasks_capture_their_own_bodies() {
    let parsed = parse_fixture("systemverilog/module_funcs.sv");
    for (name, start, end) in [("addone", 2, 4), ("dowork", 6, 8)] {
        let symbol = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing SystemVerilog {name}"));
        assert_eq!((symbol.start_line, symbol.end_line), (start, end));
        assert_ne!(
            symbol.signature,
            "module top(input logic a, output logic b);"
        );
    }
}

#[test]
fn postgres_trigger_captures_only_the_trigger_name() {
    let parsed = parse_fixture("sql/trigger_pg.sql");
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "users_audit" && symbol.kind == SymbolKind::Function })
    );
    assert!(!parsed.symbols.iter().any(|symbol| {
        symbol.kind == SymbolKind::Function
            && symbol.start_line == 3
            && matches!(symbol.name.as_str(), "users" | "audit_fn")
    }));
}

#[test]
fn sqlite_trigger_tail_loss_keeps_prefix_and_discloses_partial_parse() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/sql/trigger_sqlite.sql");
    let source = std::fs::read_to_string(&fixture).unwrap();
    let parsed = parse_source(Path::new("sql/trigger_sqlite.sql"), &source).unwrap();
    assert!(parsed.has_syntax_errors, "fixture must exercise the grammar boundary");
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "a1"));
    assert!(parsed.unparsable_skip("sql/trigger_sqlite.sql").is_none());
    let partial = parsed
        .partial_parse_skip("sql/trigger_sqlite.sql")
        .expect("trigger tail loss is disclosed without hiding the valid prefix");
    assert_eq!(partial.reason_code, SkipReasonCode::ParseError);
    assert!(partial.reason.starts_with("partial_parse:"));

    let batch = parse_batch(&[(Path::new("sql/trigger_sqlite.sql"), &source)]);
    assert_eq!(batch.files.len(), 1, "partial parse prefix remains usable");
    assert_eq!(batch.skipped.len(), 1, "coverage warning travels beside the file");
    assert!(batch.skipped[0].reason.starts_with("partial_parse:"));
}

#[test]
fn objective_c_superclasses_and_categories_do_not_become_classes() {
    let parsed = parse_fixture("objc/inherit_category.m");
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "NSObject")
    );
    assert!(!parsed.symbols.iter().any(|symbol| symbol.name == "Extra"));
    assert_eq!(
        parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "Base")
            .count(),
        4,
        "interface, category, implementation and implementation category belong to Base"
    );
}
