use std::collections::HashMap;
use std::collections::HashSet;

use nestweaver_parser::RawSymbol;
use nestweaver_schema::{ResolvedType, SymbolKind};

/// Tier 1: Infer types from constructor calls.
/// When a Call reference resolves to a Class symbol, the call site's variable
/// has the type of that class.
pub fn infer_constructor_types(
    files: &[(String, Vec<RawSymbol>)],
    resolved_calls: &[(String, String)], // (call_name, target_symbol_name)
) -> HashMap<String, ResolvedType> {
    let mut type_map = HashMap::new();

    let class_names: HashSet<&str> = files
        .iter()
        .flat_map(|(_, syms)| syms.iter())
        .filter(|s| matches!(s.kind, SymbolKind::Class | SymbolKind::Enum))
        .map(|s| s.name.as_str())
        .collect();

    for (call_site_id, target_name) in resolved_calls {
        if class_names.contains(target_name.as_str()) {
            type_map.insert(
                call_site_id.clone(),
                ResolvedType {
                    type_name: target_name.clone(),
                    resolution_tier: 1,
                    confidence: 0.85,
                },
            );
        }
    }

    type_map
}

/// Tier 2: Propagate types through assignment chains using fixpoint iteration.
/// Each iteration propagates known types to untyped targets.
/// Confidence decays by 0.05 per iteration.
/// Capped at `max_iterations` to prevent pathological cases.
pub fn propagate_types(
    initial_types: &mut HashMap<String, ResolvedType>,
    assignments: &[(String, String)], // (target_var_id, source_var_id)
    max_iterations: usize,
) -> usize {
    let mut iterations = 0;
    loop {
        let mut changed = false;
        for (target, source) in assignments {
            if !initial_types.contains_key(target)
                && let Some(source_type) = initial_types.get(source).cloned()
            {
                let confidence = (source_type.confidence - 0.05).max(0.0);
                initial_types.insert(
                    target.clone(),
                    ResolvedType {
                        type_name: source_type.type_name,
                        resolution_tier: 2,
                        confidence,
                    },
                );
                changed = true;
            }
        }
        iterations += 1;
        if !changed || iterations >= max_iterations {
            break;
        }
    }
    iterations
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::Visibility;

    fn make_class(name: &str) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            signature: format!("class {name}"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
        }
    }

    fn make_function(name: &str) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_line: 1,
            signature: format!("function {name}()"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
        }
    }

    #[test]
    fn constructor_inference_detects_class_call() {
        let files = vec![(
            "src/main.js".to_string(),
            vec![make_function("main"), make_class("Foo")],
        )];
        let calls = vec![("var_x".to_string(), "Foo".to_string())];
        let types = infer_constructor_types(&files, &calls);
        assert_eq!(types.len(), 1);
        let t = &types["var_x"];
        assert_eq!(t.type_name, "Foo");
        assert_eq!(t.resolution_tier, 1);
        assert!((t.confidence - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn constructor_inference_ignores_function_call() {
        let files = vec![("src/main.js".to_string(), vec![make_function("helper")])];
        let calls = vec![("var_x".to_string(), "helper".to_string())];
        let types = infer_constructor_types(&files, &calls);
        assert!(types.is_empty());
    }

    #[test]
    fn fixpoint_propagates_type() {
        let mut types = HashMap::new();
        types.insert(
            "a".to_string(),
            ResolvedType {
                type_name: "Foo".to_string(),
                resolution_tier: 1,
                confidence: 0.85,
            },
        );
        let assignments = vec![
            ("b".to_string(), "a".to_string()),
            ("c".to_string(), "b".to_string()),
        ];
        let iterations = propagate_types(&mut types, &assignments, 10);
        assert!(iterations <= 3, "should converge quickly");
        assert_eq!(types.len(), 3);
        assert_eq!(types["b"].type_name, "Foo");
        assert_eq!(types["b"].resolution_tier, 2);
        assert!((types["b"].confidence - 0.80).abs() < f32::EPSILON);
        assert_eq!(types["c"].type_name, "Foo");
        assert!((types["c"].confidence - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn fixpoint_caps_iterations() {
        let mut types = HashMap::new();
        // No initial types, assignments form a cycle that can never resolve
        let assignments = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ];
        let iterations = propagate_types(&mut types, &assignments, 10);
        assert_eq!(
            iterations, 1,
            "should stop after 1 iteration with no changes"
        );
        assert!(types.is_empty());
    }
}
