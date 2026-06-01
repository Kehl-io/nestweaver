use std::collections::HashMap;
use std::collections::HashSet;

use nestweaver_parser::RawSymbol;
use nestweaver_schema::{Language, ResolvedType, SymbolKind};

use crate::type_extractors::{BindingSource, TypeBinding, extract_bindings};

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

/// Per-file type environment: maps (variable_name, scope_line) → type_name.
/// Built by running all four inference tiers, then the fixpoint loop.
pub struct TypeEnvironment {
    bindings: HashMap<(String, u32), TypeBinding>,
}

impl TypeEnvironment {
    /// Build a type environment for a single file.
    pub fn build(source: &str, language: Language, symbols: &[RawSymbol]) -> Self {
        // Tiers 0-2: annotations, constructors, self/this
        let mut bindings = extract_bindings(source, language, symbols);

        // Tier 3: Assignment chain fixpoint
        let assignments = extract_assignments(source);
        propagate_assignments(&mut bindings, &assignments, 10);

        Self { bindings }
    }

    /// Look up the type of a variable at a given scope.
    /// Searches backwards from `at_line` for the nearest binding.
    pub fn lookup(&self, variable: &str, at_line: u32) -> Option<&TypeBinding> {
        // Exact match first
        if let Some(binding) = self.bindings.get(&(variable.to_string(), at_line)) {
            return Some(binding);
        }
        // Search backwards for nearest binding
        self.bindings
            .iter()
            .filter(|((name, line), _)| name == variable && *line <= at_line)
            .max_by_key(|((_, line), _)| *line)
            .map(|(_, binding)| binding)
    }

    /// Look up self/this type at a given line.
    pub fn lookup_self(&self, at_line: u32) -> Option<&TypeBinding> {
        for keyword in &["self", "this", "$this"] {
            if let Some(b) = self.lookup(keyword, at_line) {
                return Some(b);
            }
        }
        None
    }

    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }

    /// Construct a `TypeEnvironment` from pre-built bindings (for testing).
    pub fn from_bindings(entries: Vec<(String, u32, TypeBinding)>) -> Self {
        let mut bindings = HashMap::new();
        for (name, line, binding) in entries {
            bindings.insert((name, line), binding);
        }
        Self { bindings }
    }
}

/// Extract simple assignment patterns from source.
fn extract_assignments(source: &str) -> Vec<((String, u32), (String, u32))> {
    let mut assignments = Vec::new();
    for (line_num, line) in source.lines().enumerate() {
        let line_num = (line_num + 1) as u32;
        let trimmed = line.trim();
        if let Some(eq_pos) = trimmed.find('=') {
            // Skip ==, !=, <=, >=, =>
            if eq_pos > 0 {
                let prev = trimmed.as_bytes()[eq_pos - 1];
                if matches!(prev, b'!' | b'<' | b'>' | b'=') {
                    continue;
                }
            }
            if trimmed.as_bytes().get(eq_pos + 1) == Some(&b'=')
                || trimmed.as_bytes().get(eq_pos + 1) == Some(&b'>')
            {
                continue;
            }
            let lhs = trimmed[..eq_pos]
                .trim()
                .trim_start_matches("let ")
                .trim_start_matches("mut ")
                .trim_start_matches("const ")
                .trim_start_matches("var ")
                .trim();
            let rhs = trimmed[eq_pos + 1..].trim().trim_end_matches(';');
            // Only simple identifier-to-identifier assignments
            if !lhs.is_empty()
                && !rhs.is_empty()
                && rhs.chars().all(|c| c.is_alphanumeric() || c == '_')
                && lhs.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !lhs.contains(':')
            {
                assignments.push(((lhs.to_string(), line_num), (rhs.to_string(), line_num)));
            }
        }
    }
    assignments
}

/// Propagate type bindings through assignment chains (fixpoint).
fn propagate_assignments(
    bindings: &mut HashMap<(String, u32), TypeBinding>,
    assignments: &[((String, u32), (String, u32))],
    max_iterations: usize,
) {
    for _ in 0..max_iterations {
        let mut changed = false;
        for ((target_name, target_line), (source_name, _)) in assignments {
            let target_key = (target_name.clone(), *target_line);
            if bindings.contains_key(&target_key) {
                continue;
            }
            let source_type = bindings
                .iter()
                .filter(|((name, line), _)| name == source_name && *line <= *target_line)
                .max_by_key(|((_, line), _)| *line)
                .map(|(_, b)| b.clone());
            if let Some(src) = source_type {
                bindings.insert(
                    target_key,
                    TypeBinding {
                        type_name: src.type_name,
                        line: *target_line,
                        confidence: (src.confidence - 0.05).max(0.0),
                        source: BindingSource::Assignment,
                    },
                );
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
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
            end_line: 1,
            signature: format!("class {name}"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
        }
    }

    fn make_function(name: &str) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 1,
            signature: format!("function {name}()"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
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

    // ── TypeEnvironment tests ──────────────────────────────────────────

    #[test]
    fn type_env_rust_annotation_lookup() {
        let source = "fn main() {\n    let store: GraphStore = GraphStore::new();\n    store.compute_pagerank();\n}\n";
        let symbols = vec![make_function("main")];
        let env = TypeEnvironment::build(source, Language::Rust, &symbols);
        let binding = env.lookup("store", 3);
        assert!(binding.is_some(), "should find 'store' binding");
        assert_eq!(binding.unwrap().type_name, "GraphStore");
    }

    #[test]
    fn type_env_self_lookup() {
        let mut method = make_function("compute_pagerank");
        method.kind = SymbolKind::Method;
        method.parent_name = Some("GraphStore".to_string());
        method.start_line = 5;
        let env = TypeEnvironment::build("", Language::Rust, &[method]);
        let binding = env.lookup_self(6);
        assert!(binding.is_some());
        assert_eq!(binding.unwrap().type_name, "GraphStore");
        assert_eq!(binding.unwrap().confidence, 1.0);
    }

    #[test]
    fn type_env_assignment_propagation() {
        let source = "let store = GraphStore::new();\nlet alias = store;\n";
        let env = TypeEnvironment::build(source, Language::Rust, &[]);
        let binding = env.lookup("alias", 2);
        assert!(binding.is_some(), "alias should get store's type");
        assert_eq!(binding.unwrap().type_name, "GraphStore");
        assert!(binding.unwrap().confidence < 0.90);
    }

    #[test]
    fn type_env_binding_count() {
        let source = "let x: Foo = Foo::new();\nlet y: Bar = Bar::new();\n";
        let env = TypeEnvironment::build(source, Language::Rust, &[]);
        assert!(env.binding_count() >= 2);
    }
}
