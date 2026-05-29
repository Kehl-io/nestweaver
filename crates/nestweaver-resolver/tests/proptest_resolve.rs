use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_resolver::resolve_references;
use nestweaver_schema::{Language, SymbolKind, Visibility};
use proptest::prelude::*;

// ── Arbitrary strategies ──────────────────────────────────────────────────

fn arb_symbol_kind() -> impl Strategy<Value = SymbolKind> {
    prop_oneof![
        Just(SymbolKind::Function),
        Just(SymbolKind::Class),
        Just(SymbolKind::Method),
        Just(SymbolKind::Interface),
        Just(SymbolKind::Trait),
        Just(SymbolKind::Enum),
        Just(SymbolKind::Module),
        Just(SymbolKind::Extension),
        Just(SymbolKind::Constant),
        Just(SymbolKind::Property),
        Just(SymbolKind::TypeAlias),
        Just(SymbolKind::Variable),
    ]
}

fn arb_ref_kind() -> impl Strategy<Value = ReferenceKind> {
    prop_oneof![
        Just(ReferenceKind::Call),
        Just(ReferenceKind::Import),
        Just(ReferenceKind::Extends),
        Just(ReferenceKind::Implements),
        Just(ReferenceKind::Includes),
        Just(ReferenceKind::Uses),
    ]
}

fn arb_visibility() -> impl Strategy<Value = Visibility> {
    prop_oneof![
        Just(Visibility::Public),
        Just(Visibility::Internal),
        Just(Visibility::Protected),
        Just(Visibility::Private),
        Just(Visibility::Inferred),
    ]
}

fn arb_language() -> impl Strategy<Value = Language> {
    prop_oneof![
        Just(Language::JavaScript),
        Just(Language::TypeScript),
        Just(Language::Java),
        Just(Language::Go),
        Just(Language::Python),
        Just(Language::Cpp),
        Just(Language::Rust),
        Just(Language::Kotlin),
        Just(Language::CSharp),
        Just(Language::Php),
        Just(Language::Ruby),
        Just(Language::Swift),
        Just(Language::C),
        Just(Language::Dart),
        Just(Language::Cobol),
        Just(Language::Vue),
        Just(Language::Svelte),
        Just(Language::Astro),
        Just(Language::SystemVerilog),
    ]
}

fn arb_symbol() -> impl Strategy<Value = RawSymbol> {
    (
        "[a-zA-Z_][a-zA-Z0-9_]{0,19}",
        arb_symbol_kind(),
        1..1000u32,
        arb_visibility(),
    )
        .prop_map(|(name, kind, start_line, visibility)| RawSymbol {
            name,
            kind,
            start_line,
            end_line: start_line,
            signature: String::new(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility,
            type_info: None,
        })
}

fn arb_reference() -> impl Strategy<Value = RawReference> {
    ("[a-zA-Z_][a-zA-Z0-9_]{0,19}", arb_ref_kind(), 1..1000u32).prop_map(
        |(name, kind, start_line)| RawReference {
            name,
            kind,
            start_line,
            context: String::new(),
        },
    )
}

fn arb_file() -> impl Strategy<Value = (String, Vec<RawSymbol>, Vec<RawReference>)> {
    (
        "src/[a-z]{1,8}\\.[a-z]{1,4}",
        prop::collection::vec(arb_symbol(), 0..10),
        prop::collection::vec(arb_reference(), 0..10),
    )
}

fn arb_files() -> impl Strategy<Value = Vec<(String, Vec<RawSymbol>, Vec<RawReference>)>> {
    prop::collection::vec(arb_file(), 1..6)
}

// ── Property tests ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// The resolver must never panic regardless of input shape.
    #[test]
    fn resolver_never_panics(
        files in arb_files(),
        language in arb_language(),
    ) {
        let _ = resolve_references(&files, language, "repo:test:proptest");
    }

    /// Every resolved edge target must either be a valid symbol UID
    /// (starting with "sym:") or be prefixed with "unresolved:".
    #[test]
    fn resolved_targets_are_valid_or_unresolved(
        files in arb_files(),
        language in arb_language(),
    ) {
        let edges = resolve_references(&files, language, "repo:test:proptest");

        for edge in &edges {
            let is_unresolved = edge.target_uid.starts_with("unresolved:");
            let is_symbol_uid = edge.target_uid.starts_with("sym:");
            prop_assert!(
                is_unresolved || is_symbol_uid,
                "target_uid {:?} has unexpected prefix (expected 'sym:' or 'unresolved:')",
                edge.target_uid,
            );
        }
    }

    /// All confidence scores must be in the range [0.0, 1.0].
    #[test]
    fn confidence_scores_in_range(
        files in arb_files(),
        language in arb_language(),
    ) {
        let edges = resolve_references(&files, language, "repo:test:proptest");

        for edge in &edges {
            prop_assert!(
                (0.0..=1.0).contains(&edge.confidence),
                "confidence {} out of range [0.0, 1.0] for edge {:?} -> {:?}",
                edge.confidence,
                edge.source_uid,
                edge.target_uid,
            );
        }
    }

    /// Resolution is deterministic: the same input always produces the same output.
    #[test]
    fn resolution_is_deterministic(
        files in arb_files(),
        language in arb_language(),
    ) {
        let edges_a = resolve_references(&files, language, "repo:test:proptest");
        let edges_b = resolve_references(&files, language, "repo:test:proptest");

        prop_assert_eq!(edges_a.len(), edges_b.len(), "edge count differs between runs");

        for (a, b) in edges_a.iter().zip(edges_b.iter()) {
            prop_assert_eq!(&a.source_uid, &b.source_uid);
            prop_assert_eq!(&a.target_uid, &b.target_uid);
            prop_assert_eq!(a.edge_type, b.edge_type);
            prop_assert!(
                (a.confidence - b.confidence).abs() < f32::EPSILON,
                "confidence differs: {} vs {}",
                a.confidence,
                b.confidence,
            );
        }
    }
}
