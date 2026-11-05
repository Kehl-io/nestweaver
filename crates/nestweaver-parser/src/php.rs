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

/// Parser-owned facts for one named PHP class. Exact declaration/call byte
/// regions keep repeated names on a line or in different namespaces distinct.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhpClassFacts {
    pub parent: Option<String>,
    pub is_final: bool,
    pub has_traits: bool,
    pub methods: Vec<PhpMethodFacts>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhpMethodFacts {
    pub name: String,
    pub start_line: u32,
    pub end_line: u32,
    pub scope: LexicalScope,
    pub is_static: bool,
    pub is_final: bool,
    pub is_abstract: bool,
}

pub(crate) fn collect_class_facts(
    root: Node<'_>,
    source: &[u8],
    references: &mut Vec<RawReference>,
) {
    let regions = namespace_regions(root, source);
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
        if !matches!(
            node.kind(),
            "class_declaration" | "enum_declaration" | "anonymous_class"
        ) {
            continue;
        }
        // Empty identity marks an anonymous lexical class boundary. It must
        // hide an outer named class for self/parent/static dispatch.
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("");
        let namespace = namespace_for_position(&regions, node.start_byte()).unwrap_or("");
        let qualified = if name.is_empty() || namespace.is_empty() {
            name.to_owned()
        } else {
            format!("{namespace}\\{name}")
        };
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        let parent = children
            .iter()
            .find(|n| n.kind() == "base_clause")
            .and_then(|base| base.named_child(0))
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_owned);
        let is_final = node.kind() == "enum_declaration"
            || children.iter().any(|n| n.kind() == "final_modifier");
        let body = node.child_by_field_name("body");
        let mut members = Vec::new();
        if let Some(body) = body {
            let mut cursor = body.walk();
            members.extend(body.named_children(&mut cursor));
        }
        let methods = members
            .iter()
            .filter(|n| n.kind() == "method_declaration")
            .filter_map(|method| {
                let name = method
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?
                    .to_owned();
                let mut cursor = method.walk();
                let modifiers: Vec<_> = method
                    .named_children(&mut cursor)
                    .map(|n| n.kind())
                    .collect();
                Some(PhpMethodFacts {
                    name,
                    start_line: method.start_position().row as u32 + 1,
                    end_line: method.end_position().row as u32 + 1,
                    scope: scope(method.start_byte(), method.end_byte(), method.start_byte()),
                    is_static: modifiers.contains(&"static_modifier"),
                    is_final: modifiers.contains(&"final_modifier"),
                    is_abstract: modifiers.contains(&"abstract_modifier")
                        || method.child_by_field_name("body").is_none(),
                })
            })
            .collect();
        let facts = PhpClassFacts {
            parent,
            is_final,
            has_traits: members
                .iter()
                .any(|member| member.kind() == "use_declaration"),
            methods,
        };
        references.push(RawReference {
            name: qualified,
            kind: ReferenceKind::PhpClassDefinition,
            start_line: node.start_position().row as u32 + 1,
            context: serde_json::to_string(&facts).unwrap_or_default(),
            receiver: None,
            scope: Some(scope(node.start_byte(), node.end_byte(), node.start_byte())),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_source;
    use std::path::Path;

    #[test]
    fn php_static_calls_keep_receivers_and_ast_owned_class_flags() {
        let source = "<?php\nnamespace App;\nfinal class Child extends \\Lib\\Base {\n use SomeTrait;\n final public static function run() { self::m(); static::m(); parent::m(); \\Lib\\Base::m(); $class::m(); }\n public function instance() {}\n}\n";
        let parsed = parse_source(Path::new("static.php"), source).unwrap();
        assert!(!parsed.has_syntax_errors);
        for receiver in ["self", "static", "parent", "\\Lib\\Base", "$class"] {
            let position = source.find(&format!("{receiver}::m()")).unwrap();
            for kind in [ReferenceKind::Call, ReferenceKind::PhpStaticCall] {
                assert!(
                    parsed.references.iter().any(|r| r.kind == kind
                        && r.name == "m"
                        && r.receiver.as_deref() == Some(receiver)
                        && r.scope.is_some_and(|scope| scope.position == position)),
                    "{receiver}: {:#?}",
                    parsed.references
                );
            }
        }
        let class = parsed
            .references
            .iter()
            .find(|r| r.kind == ReferenceKind::PhpClassDefinition && r.name == "App\\Child")
            .unwrap();
        let facts: PhpClassFacts = serde_json::from_str(&class.context).unwrap();
        assert!(facts.is_final && facts.has_traits);
        assert_eq!(facts.parent.as_deref(), Some("\\Lib\\Base"));
        let run = facts.methods.iter().find(|m| m.name == "run").unwrap();
        assert!(run.is_static && run.is_final && !run.is_abstract);
        assert!(
            !facts
                .methods
                .iter()
                .find(|m| m.name == "instance")
                .unwrap()
                .is_static
        );
    }

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
    fn php_free_and_static_calls_keep_positions_but_instance_calls_keep_type_lookup() {
        let source = "<?php\nclass Carrier { public function method() {} public static function staticMethod() {} }\nfunction named() {}\nfunction caller(Carrier $input) {\n $bound = new Carrier();\n $bound->method();\n Carrier::staticMethod();\n $input?->method();\n named();\n Vendor\\helper();\n namespace\\local();\n}\n";
        let parsed = parse_source(Path::new("receivers.php"), source).unwrap();
        assert!(!parsed.has_syntax_errors);
        for name in ["Carrier", "method"] {
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
        let static_call = parsed
            .references
            .iter()
            .find(|r| r.kind == ReferenceKind::Call && r.name == "staticMethod")
            .unwrap();
        assert_eq!(static_call.receiver.as_deref(), Some("Carrier"));
        assert!(static_call.scope.is_some());
        // Nullsafe member calls are not currently captured by php.scm; ordinary
        // member captures retain their existing unscoped type lookup.
        assert!(
            parsed
                .references
                .iter()
                .filter(|r| r.kind == ReferenceKind::Call
                    && r.receiver.is_some()
                    && r.name != "staticMethod")
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
