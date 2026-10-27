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
