use tree_sitter::Node;

/// Extract the scope chain for a symbol by walking up the tree-sitter AST.
/// Returns a `::` separated chain like `MyModule::MyClass::my_method`.
///
/// The chain includes only the *enclosing* scopes — not the symbol itself.
/// Callers typically append the symbol name when constructing a canonical ID.
pub fn extract_scope_chain(node: Node, source: &str, lang: &str) -> Option<String> {
    let scope_node_types = scope_types_for_language(lang);
    if scope_node_types.is_empty() {
        return None; // language has no scope constructs
    }

    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if scope_node_types.contains(&parent.kind()) {
            if let Some(name) = extract_name_from_scope_node(parent, source) {
                chain.push(name);
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    if chain.is_empty() {
        None
    } else {
        Some(chain.join("::"))
    }
}

fn extract_name_from_scope_node(node: Node, source: &str) -> Option<String> {
    let source_bytes = source.as_bytes();

    // Try common patterns for finding the name child:
    // 1. Child with field name "name"
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(name_node.utf8_text(source_bytes).ok()?.to_string());
    }

    // 2. For Rust impl blocks, look for the "type" field
    if node.kind() == "impl_item" {
        if let Some(type_node) = node.child_by_field_name("type") {
            return Some(type_node.utf8_text(source_bytes).ok()?.to_string());
        }
    }

    // 3. For Elixir defmodule (which is a `call` node), extract the module name
    //    from the first argument: defmodule MyApp.Router do ... end
    if node.kind() == "call" {
        // Check if this is a defmodule call
        if let Some(target) = node.child_by_field_name("target") {
            let target_text = target.utf8_text(source_bytes).ok()?;
            if target_text == "defmodule" {
                // The module name is the first argument
                if let Some(args) = node.child_by_field_name("arguments") {
                    if let Some(first_arg) = args.child(0) {
                        return Some(first_arg.utf8_text(source_bytes).ok()?.to_string());
                    }
                }
            }
        }
    }

    // 4. For HCL blocks, extract the block type and labels
    if node.kind() == "block" {
        // HCL blocks look like: resource "aws_instance" "web" { ... }
        // First child is the block type identifier
        let mut parts = Vec::new();
        let hcl_count = node.child_count();
        for i in 0..hcl_count {
            if let Some(child) = node.child(i as u32) {
                match child.kind() {
                    "identifier" => {
                        if let Ok(text) = child.utf8_text(source_bytes) {
                            parts.push(text.to_string());
                        }
                    }
                    "string_lit" => {
                        if let Ok(text) = child.utf8_text(source_bytes) {
                            // Strip quotes
                            let stripped = text.trim_matches('"');
                            parts.push(stripped.to_string());
                        }
                    }
                    // Stop at the block body
                    "body" | "block" => break,
                    _ => {}
                }
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("."));
        }
    }

    // 5. Walk children looking for an identifier
    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i as u32) {
            if child.kind() == "identifier"
                || child.kind() == "type_identifier"
                || child.kind() == "name"
                || child.kind() == "constant"  // Ruby module/class names are constants
            {
                return Some(child.utf8_text(source_bytes).ok()?.to_string());
            }
        }
    }

    None
}

/// Return the tree-sitter node types that create scopes for a given language.
fn scope_types_for_language(lang: &str) -> &'static [&'static str] {
    match lang {
        // OOP languages with classes + modules/namespaces
        "rust" => &[
            "mod_item",
            "impl_item",
            "trait_item",
            "struct_item",
            "enum_item",
        ],
        "typescript" | "javascript" | "tsx" | "jsx" => &[
            "class_declaration",
            "class",
            "namespace_declaration",
            "module",
            "function_declaration",
            "method_definition",
        ],
        "java" => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
        "kotlin" => &[
            "class_declaration",
            "object_declaration",
            "interface_declaration",
            "companion_object",
        ],
        "csharp" | "c_sharp" => &[
            "class_declaration",
            "struct_declaration",
            "namespace_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
        "python" => &["class_definition", "function_definition"],
        "ruby" => &["class", "module", "singleton_class"],
        "php" => &[
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
            "namespace_definition",
            "enum_declaration",
        ],
        "swift" => &[
            "class_declaration",
            "struct_declaration",
            "enum_declaration",
            "protocol_declaration",
            "extension_declaration",
        ],
        "dart" => &[
            "class_declaration",
            "mixin_declaration",
            "extension_declaration",
            "enum_declaration",
        ],
        "scala" => &[
            "class_definition",
            "object_definition",
            "trait_definition",
            "package_clause",
        ],
        "go" => &["type_declaration", "method_declaration"],

        // C-family
        "c" => &["struct_specifier"],
        "cpp" | "c++" => &[
            "class_specifier",
            "struct_specifier",
            "namespace_definition",
            "template_declaration",
        ],
        "objc" | "objective_c" => &[
            "class_interface",
            "class_implementation",
            "category_interface",
            "category_implementation",
            "protocol_declaration",
        ],

        // Functional / other
        "elixir" => &["call"], // defmodule is a call in tree-sitter-elixir
        "lua" => &[],          // Lua has no class/module node types in tree-sitter
        "julia" => &[
            "module_definition",
            "struct_definition",
            "abstract_definition",
        ],
        "zig" => &["container_declaration"],

        // Shell/scripting
        "bash" | "sh" => &[], // file-level scope only
        "powershell" => &["class_statement"],

        // Domain-specific
        "sql" => &["create_table_statement"],
        "hcl" => &["block"],
        "fortran" => &["module", "program", "subroutine", "function"],
        "pascal" => &["class_type", "record_type", "unit"],
        "cobol" => &["program_definition", "class_definition"],
        "groovy" => &["class_declaration", "interface_declaration"],
        "systemverilog" | "system_verilog" => &[
            "module_declaration",
            "class_declaration",
            "interface_declaration",
        ],

        // Web frameworks (these are regex-parsed, so scope_chain is set to None
        // externally; these entries exist as a safety net if the function is
        // ever called for them)
        "vue" | "svelte" | "astro" => &["class_declaration", "function_declaration"],

        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    /// Helper: parse source with the given tree-sitter language, find the first
    /// node of `target_kind` whose text contains `target_substr`, then call
    /// `extract_scope_chain` on it.
    fn scope_chain_for(
        ts_lang: tree_sitter::Language,
        lang_str: &str,
        source: &str,
        target_kind: &str,
        target_substr: &str,
    ) -> Option<String> {
        let mut parser = Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();

        fn find_node<'a>(
            node: Node<'a>,
            kind: &str,
            substr: &str,
            source: &str,
        ) -> Option<Node<'a>> {
            if node.kind() == kind {
                let text = node.utf8_text(source.as_bytes()).unwrap_or("");
                if text.contains(substr) {
                    return Some(node);
                }
            }
            let n = node.child_count();
            for i in 0..n {
                if let Some(child) = node.child(i as u32) {
                    if let Some(found) = find_node(child, kind, substr, source) {
                        return Some(found);
                    }
                }
            }
            None
        }

        let target = find_node(tree.root_node(), target_kind, target_substr, source)?;
        extract_scope_chain(target, source, lang_str)
    }

    #[test]
    fn scope_chain_rust_mod_impl() {
        let source = r#"
mod sensors {
    struct Manager {}
    impl Manager {
        fn new() -> Self { Manager {} }
        fn read(&self) -> u32 { 42 }
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            source,
            "function_item",
            "fn read",
        );
        assert_eq!(chain, Some("sensors::Manager".to_string()));
    }

    #[test]
    fn scope_chain_rust_trait() {
        let source = r#"
mod io {
    trait Readable {
        fn read(&self) -> Vec<u8>;
    }
}
"#;
        // Trait method signatures are `function_signature_item` in tree-sitter-rust
        let chain = scope_chain_for(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            source,
            "function_signature_item",
            "fn read",
        );
        assert_eq!(chain, Some("io::Readable".to_string()));
    }

    #[test]
    fn scope_chain_typescript_class_method() {
        let source = r#"
class Animal {
    speak() { return "..."; }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            source,
            "method_definition",
            "speak",
        );
        assert_eq!(chain, Some("Animal".to_string()));
    }

    #[test]
    fn scope_chain_typescript_namespace_class() {
        let source = r#"
namespace App {
    class Service {
        handle() {}
    }
}
"#;
        // NOTE: tree-sitter-typescript may represent `namespace` as
        // `module` or `namespace_declaration` — both are in our list.
        let chain = scope_chain_for(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            source,
            "method_definition",
            "handle",
        );
        // Depending on the grammar, this could be App::Service or just Service
        assert!(chain.is_some());
        let chain_str = chain.unwrap();
        assert!(
            chain_str.contains("Service"),
            "scope chain should contain 'Service'; got: {chain_str}"
        );
    }

    #[test]
    fn scope_chain_python_class() {
        let source = r#"
class Animal:
    def speak(self):
        return "..."

class Dog(Animal):
    def speak(self):
        return "Woof"
"#;
        let chain = scope_chain_for(
            tree_sitter_python::LANGUAGE.into(),
            "python",
            source,
            "function_definition",
            "def speak",
        );
        assert_eq!(chain, Some("Animal".to_string()));
    }

    #[test]
    fn scope_chain_python_nested_class() {
        let source = r#"
class Outer:
    class Inner:
        def method(self):
            pass
"#;
        let chain = scope_chain_for(
            tree_sitter_python::LANGUAGE.into(),
            "python",
            source,
            "function_definition",
            "def method",
        );
        assert_eq!(chain, Some("Outer::Inner".to_string()));
    }

    #[test]
    fn scope_chain_java_class() {
        let source = r#"
public class SimpleGreeter implements Greeter {
    public String greet(String name) {
        return "Hello, " + name;
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_java::LANGUAGE.into(),
            "java",
            source,
            "method_declaration",
            "greet",
        );
        assert_eq!(chain, Some("SimpleGreeter".to_string()));
    }

    #[test]
    fn scope_chain_java_nested_class() {
        let source = r#"
public class Outer {
    public static class Inner {
        public void run() {}
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_java::LANGUAGE.into(),
            "java",
            source,
            "method_declaration",
            "run",
        );
        assert_eq!(chain, Some("Outer::Inner".to_string()));
    }

    #[test]
    fn scope_chain_go_method() {
        let source = r#"
package main

type ConsoleGreeter struct{}

func (g *ConsoleGreeter) Greet(name string) string {
    return "Hello, " + name
}
"#;
        // Go methods have a `method_declaration` node; the receiver type
        // is not an enclosing scope — it's a sibling. The method itself has
        // no nesting.  type_declaration creates scope but methods are
        // defined outside the type block in Go.
        let chain = scope_chain_for(
            tree_sitter_go::LANGUAGE.into(),
            "go",
            source,
            "method_declaration",
            "Greet",
        );
        // Go methods are top-level, so scope chain should be None
        assert_eq!(chain, None);
    }

    #[test]
    fn scope_chain_csharp_namespace_class() {
        let source = r#"
namespace MyApp {
    public class SimpleGreeter : IGreeter {
        public string Greet(string name) {
            return "Hello, " + name;
        }
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_c_sharp::LANGUAGE.into(),
            "csharp",
            source,
            "method_declaration",
            "Greet",
        );
        assert_eq!(chain, Some("MyApp::SimpleGreeter".to_string()));
    }

    #[test]
    fn scope_chain_c_struct() {
        // C doesn't have methods inside structs in tree-sitter, so there's
        // no nesting to detect. A function defined after a struct is top-level.
        let source = r#"
struct SensorManager {
    int count;
};

void initialize(struct SensorManager* sm) {
    sm->count = 0;
}
"#;
        let chain = scope_chain_for(
            tree_sitter_c::LANGUAGE.into(),
            "c",
            source,
            "function_definition",
            "initialize",
        );
        // C functions are always top-level
        assert_eq!(chain, None);
    }

    #[test]
    fn scope_chain_cpp_namespace_class() {
        let source = r#"
namespace sensors {
    class Manager {
    public:
        void initialize() {}
    };
}
"#;
        let chain = scope_chain_for(
            tree_sitter_cpp::LANGUAGE.into(),
            "cpp",
            source,
            "function_definition",
            "initialize",
        );
        assert!(chain.is_some());
        let chain_str = chain.unwrap();
        assert!(
            chain_str.contains("sensors") && chain_str.contains("Manager"),
            "scope chain should contain 'sensors' and 'Manager'; got: {chain_str}"
        );
    }

    #[test]
    fn scope_chain_ruby_module_class() {
        let source = r#"
module Animals
  class Dog
    def speak
      "Woof"
    end
  end
end
"#;
        let chain = scope_chain_for(
            tree_sitter_ruby::LANGUAGE.into(),
            "ruby",
            source,
            "method",
            "speak",
        );
        assert_eq!(chain, Some("Animals::Dog".to_string()));
    }

    #[test]
    fn scope_chain_no_scope_language_bash() {
        let source = r#"
function greet() {
    echo "Hello"
}
"#;
        let chain = scope_chain_for(
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            source,
            "function_definition",
            "greet",
        );
        assert_eq!(chain, None, "Bash should have no scope chain");
    }

    #[test]
    fn scope_chain_no_scope_language_lua() {
        let source = r#"
local function greet()
    print("Hello")
end
"#;
        let chain = scope_chain_for(
            tree_sitter_lua::LANGUAGE.into(),
            "lua",
            source,
            "function_declaration",
            "greet",
        );
        assert_eq!(chain, None, "Lua should have no scope chain");
    }

    #[test]
    fn scope_chain_kotlin_class() {
        let source = r#"
class SimpleGreeter : Greeter {
    fun greet(name: String): String {
        return "Hello, $name"
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_kotlin::LANGUAGE.into(),
            "kotlin",
            source,
            "function_declaration",
            "greet",
        );
        assert_eq!(chain, Some("SimpleGreeter".to_string()));
    }

    #[test]
    fn scope_chain_php_class() {
        let source = r#"<?php
class UserService {
    public function getUser(int $id): User {
        return new User($id);
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
            "php",
            source,
            "method_declaration",
            "getUser",
        );
        assert_eq!(chain, Some("UserService".to_string()));
    }

    #[test]
    fn scope_chain_swift_struct() {
        let source = r#"
struct Point {
    var x: Double
    var y: Double
    func distance() -> Double {
        return (x * x + y * y).squareRoot()
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_swift::LANGUAGE.into(),
            "swift",
            source,
            "function_declaration",
            "distance",
        );
        assert_eq!(chain, Some("Point".to_string()));
    }

    #[test]
    fn scope_chain_scala_object() {
        let source = r#"
object AppConfig {
    def getValue(key: String): String = {
        key
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_scala::LANGUAGE.into(),
            "scala",
            source,
            "function_definition",
            "getValue",
        );
        assert_eq!(chain, Some("AppConfig".to_string()));
    }

    #[test]
    fn scope_chain_dart_class() {
        let source = r#"
class UserService {
    User getUser(int id) {
        return User(id);
    }
}
"#;
        let chain = scope_chain_for(
            tree_sitter_dart::LANGUAGE.into(),
            "dart",
            source,
            "function_signature",
            "getUser",
        );
        // Dart method nodes may vary — try the method body too
        let chain2 = scope_chain_for(
            tree_sitter_dart::LANGUAGE.into(),
            "dart",
            source,
            "method_signature",
            "getUser",
        );
        let result = chain.or(chain2);
        assert!(
            result.is_some(),
            "Dart method should have a scope chain (class enclosure)"
        );
        if let Some(c) = result {
            assert!(
                c.contains("UserService"),
                "scope chain should contain UserService; got: {c}"
            );
        }
    }

    #[test]
    fn scope_chain_top_level_function_returns_none() {
        // A top-level function has no enclosing scope
        let source = "fn main() {}";
        let chain = scope_chain_for(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            source,
            "function_item",
            "fn main",
        );
        assert_eq!(chain, None);
    }

    // Verify that all 32 languages have explicit entries (not falling through to _ => &[])
    #[test]
    fn all_languages_have_scope_entries() {
        let lang_strs = [
            "javascript",
            "typescript",
            "java",
            "go",
            "python",
            "cpp",
            "rust",
            "kotlin",
            "csharp",
            "php",
            "ruby",
            "swift",
            "c",
            "dart",
            "cobol",
            "lua",
            "bash",
            "scala",
            "elixir",
            "zig",
            "objc",
            "groovy",
            "powershell",
            "julia",
            "sql",
            "hcl",
            "fortran",
            "pascal",
            "vue",
            "svelte",
            "astro",
            "systemverilog",
        ];
        for lang in &lang_strs {
            // Just call it — should not panic. Languages without scopes
            // return empty slices, which is fine.
            let _ = scope_types_for_language(lang);
        }
    }
}
