//! PHP's semicolon namespaces scope following siblings, not AST descendants.
//! Retain byte regions and import categories instead of guessing from file paths.

use tree_sitter::Node;

use crate::{LexicalScope, RawReference, ReferenceKind};

fn scope(start: usize, end: usize, position: usize) -> LexicalScope {
    LexicalScope {
        start,
        end,
        position,
        initialized_at: start,
        hoisted_var: false,
    }
}

pub(crate) fn namespace_regions(root: Node<'_>, source: &[u8]) -> Vec<RawReference> {
    let mut cursor = root.walk();
    let declarations: Vec<_> = root
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "namespace_definition")
        .collect();
    declarations
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let (start, end) = node
                .child_by_field_name("body")
                .map(|body| (body.start_byte(), body.end_byte()))
                .unwrap_or((
                    node.start_byte(),
                    declarations
                        .get(i + 1)
                        .map_or(root.end_byte(), Node::start_byte),
                ));
            RawReference {
                scope: Some(scope(start, end, node.start_byte())),
                name: node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string(),
                kind: ReferenceKind::PhpNamespace,
                start_line: node.start_position().row as u32 + 1,
                context: String::new(),
                receiver: None,
            }
        })
        .collect()
}

/// `None` is the global namespace, including an explicit `namespace { ... }`.
pub fn namespace_for_position(regions: &[RawReference], position: usize) -> Option<&str> {
    regions
        .iter()
        .filter(|r| r.kind == ReferenceKind::PhpNamespace)
        .find(|r| {
            r.scope
                .is_some_and(|s| s.start <= position && position < s.end)
        })
        .map(|r| r.name.as_str())
        .filter(|name| !name.is_empty())
}

pub(crate) fn collect_namespace_bindings(
    root: Node<'_>,
    source: &[u8],
    references: &mut Vec<RawReference>,
) {
    let regions = namespace_regions(root, source);
    let mut cursor = root.walk();
    let mut nodes: Vec<_> = root.named_children(&mut cursor).collect();
    while let Some(node) = nodes.pop() {
        if node.kind() == "namespace_use_declaration" {
            let region = regions
                .iter()
                .find_map(|r| {
                    r.scope
                        .filter(|s| s.start <= node.start_byte() && node.start_byte() < s.end)
                })
                .unwrap_or_else(|| scope(0, root.end_byte(), 0));
            let declaration_type = node.child_by_field_name("type").map(|n| n.kind());
            let group = node.child_by_field_name("body");
            let mut cursor = node.walk();
            let prefix = if group.is_some() {
                node.named_children(&mut cursor)
                    .find(|n| n.kind() == "namespace_name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
            } else {
                ""
            };
            let clauses = group.unwrap_or(node);
            let mut cursor = clauses.walk();
            for clause in clauses
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "namespace_use_clause")
            {
                let kind = match clause
                    .child_by_field_name("type")
                    .map(|n| n.kind())
                    .or(declaration_type)
                {
                    Some("const") => continue,
                    Some("function") => ReferenceKind::PhpFunctionImport,
                    _ => ReferenceKind::PhpNamespaceAlias,
                };
                let mut cursor = clause.walk();
                let Some(target) = clause
                    .named_children(&mut cursor)
                    .find(|n| matches!(n.kind(), "name" | "qualified_name"))
                    .and_then(|n| n.utf8_text(source).ok())
                else {
                    continue;
                };
                let qualified = if prefix.is_empty() {
                    target.trim_start_matches('\\').to_string()
                } else {
                    format!(
                        "{}\\{}",
                        prefix.trim_matches('\\'),
                        target.trim_start_matches('\\')
                    )
                };
                let alias = clause
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or_else(|| target.rsplit('\\').next().unwrap_or(target));
                references.push(RawReference {
                    scope: Some(scope(region.start, region.end, node.start_byte())),
                    name: alias.to_string(),
                    kind,
                    start_line: node.start_position().row as u32 + 1,
                    context: qualified,
                    receiver: None,
                });
            }
            continue;
        }
        // A closure's `use ($x)` is not a namespace use declaration.
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    references.extend(regions);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_source;
    use std::path::Path;

    #[test]
    fn php_namespace_facts_record_regions_and_import_categories() {
        let source = "<?php\nnamespace A {\nuse function Vendor\\{helper as One, next};\nuse Vendor\\{function helper as Two, const VALUE, Thing};\nfunction first() { One(); }\n}\nnamespace B {\nfunction second() { One(); }\n}\n";
        let parsed = parse_source(Path::new("regions.php"), source).unwrap();
        assert!(!parsed.has_syntax_errors);
        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::PhpFunctionImport)
            .collect();
        assert_eq!(imports.len(), 3);
        for (alias, target) in [
            ("One", "Vendor\\helper"),
            ("next", "Vendor\\next"),
            ("Two", "Vendor\\helper"),
        ] {
            assert!(
                imports
                    .iter()
                    .any(|r| r.name == alias && r.context == target)
            );
        }
        assert!(
            parsed
                .references
                .iter()
                .any(|r| r.kind == ReferenceKind::PhpNamespaceAlias
                    && r.name == "Thing"
                    && r.context == "Vendor\\Thing")
        );
        assert!(!parsed.references.iter().any(|r| r.name == "VALUE"
            && matches!(
                r.kind,
                ReferenceKind::PhpFunctionImport | ReferenceKind::PhpNamespaceAlias
            )));
        let first = source.find("One();").unwrap();
        let second = source.rfind("One();").unwrap();
        assert_eq!(namespace_for_position(&parsed.references, first), Some("A"));
        assert_eq!(
            namespace_for_position(&parsed.references, second),
            Some("B")
        );
        assert!(imports.iter().all(|r| {
            r.scope
                .is_some_and(|s| s.start <= first && first < s.end && second >= s.end)
        }));
        assert!(
            parsed
                .references
                .iter()
                .any(|r| r.kind == ReferenceKind::Call
                    && r.name == "One"
                    && r.scope.is_some_and(|s| s.position == first))
        );
        assert!(
            parsed
                .references
                .iter()
                .any(|r| r.kind == ReferenceKind::PhpFunctionCall
                    && r.name == "One"
                    && r.scope.is_some_and(|s| s.position == first))
        );
    }

    #[test]
    fn php_chained_method_and_free_function_have_distinct_admission_facts() {
        let source = "<?php\nfunction caller() { factory()->run(); factory()->factory(); }\n";
        let parsed = parse_source(Path::new("chains.php"), source).unwrap();
        assert!(!parsed.has_syntax_errors);
        // The collision witness is the actual AST, independently of synthetic
        // reference scope. Member calls intentionally have no lexical scope.
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP_ONLY.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        for (expression, method) in [
            ("factory()->run()", "run"),
            ("factory()->factory()", "factory"),
        ] {
            let position = source.find(expression).unwrap();
            let mut nodes = vec![tree.root_node()];
            let mut collision_kinds = std::collections::HashSet::new();
            while let Some(node) = nodes.pop() {
                if node.start_byte() == position {
                    collision_kinds.insert(node.kind());
                }
                let mut cursor = node.walk();
                nodes.extend(node.named_children(&mut cursor));
            }
            assert!(collision_kinds.contains("function_call_expression"));
            assert!(collision_kinds.contains("member_call_expression"));
            let outer: Vec<_> = parsed
                .references
                .iter()
                .filter(|r| {
                    r.kind == ReferenceKind::Call
                        && r.name == method
                        && r.receiver.as_deref() == Some("factory()")
                })
                .collect();
            assert_eq!(outer.len(), 1);
            assert!(
                outer[0].scope.is_none(),
                "member receivers must use ordinary type lookup"
            );
            let at_position: Vec<_> = parsed
                .references
                .iter()
                .filter(|r| r.scope.is_some_and(|scope| scope.position == position))
                .collect();
            assert!(at_position.iter().any(|r| r.kind == ReferenceKind::Call
                && r.name == "factory"
                && r.receiver.is_none()));
            let markers: Vec<_> = at_position
                .iter()
                .filter(|r| r.kind == ReferenceKind::PhpFunctionCall)
                .collect();
            assert_eq!(markers.len(), 1, "only the named free factory is admitted");
            assert_eq!(markers[0].name, "factory");
            assert!(markers[0].receiver.is_none());
        }
    }

    #[test]
    fn php_only_named_free_calls_receive_namespace_positions() {
        let source = "<?php\nclass Carrier { public function method() {} public static function staticMethod() {} }\nfunction named() {}\nfunction caller(Carrier $input) {\n $bound = new Carrier();\n $bound->method();\n Carrier::staticMethod();\n $input?->method();\n named();\n Vendor\\helper();\n namespace\\local();\n}\n";
        let parsed = parse_source(Path::new("receivers.php"), source).unwrap();
        assert!(!parsed.has_syntax_errors);
        for name in ["Carrier", "method", "staticMethod"] {
            let captures: Vec<_> = parsed
                .references
                .iter()
                .filter(|r| r.kind == ReferenceKind::Call && r.name == name)
                .collect();
            assert!(
                !captures.is_empty(),
                "missing constructor/member/static control {name}"
            );
            assert!(captures.iter().all(|r| r.scope.is_none()));
        }
        let free_markers: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::PhpFunctionCall)
            .collect();
        assert_eq!(free_markers.len(), 3);
        for marker in free_markers {
            let scope = marker.scope.unwrap();
            assert!(
                parsed
                    .references
                    .iter()
                    .any(|r| r.kind == ReferenceKind::Call
                        && r.name == marker.name
                        && r.receiver.is_none()
                        && r.scope.is_some_and(|s| s.position == scope.position))
            );
        }
        for (var_name, kind) in [
            ("$bound", crate::AstBindingKind::Constructor),
            ("$input", crate::AstBindingKind::Parameter),
        ] {
            assert!(
                parsed
                    .type_bindings
                    .iter()
                    .any(|binding| binding.var_name == var_name
                        && binding.type_name == "Carrier"
                        && binding.kind == kind
                        && binding.scope.is_none())
            );
        }
        // Nullsafe member calls are not currently captured by php.scm; any
        // present receiver capture must nevertheless remain unscoped.
        assert!(
            parsed
                .references
                .iter()
                .filter(|r| r.kind == ReferenceKind::Call && r.receiver.is_some())
                .all(|r| r.scope.is_none())
        );
    }

    #[test]
    fn php_explicit_global_scope_is_distinct_from_unknown_graph_metadata() {
        let parsed = parse_source(Path::new("global.php"), "<?php\nnamespace { function global_fn() {} }\nnamespace Named { function namespaced_fn() {} }\n").unwrap();
        assert_eq!(
            parsed
                .symbols
                .iter()
                .find(|s| s.name == "global_fn")
                .unwrap()
                .scope_chain
                .as_deref(),
            Some("")
        );
        assert_eq!(
            parsed
                .symbols
                .iter()
                .find(|s| s.name == "namespaced_fn")
                .unwrap()
                .scope_chain
                .as_deref(),
            Some("Named")
        );
    }
}
