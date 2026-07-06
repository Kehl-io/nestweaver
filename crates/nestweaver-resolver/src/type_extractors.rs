use std::collections::{HashMap, HashSet};

use nestweaver_parser::RawSymbol;
use nestweaver_schema::{Language, SymbolKind};

// ── Data types ────────────────────────────────────────────────────────────

/// A type binding discovered from source text analysis.
#[derive(Debug, Clone)]
pub struct TypeBinding {
    /// The resolved type name (e.g. `"Foo"`, `"HashMap"`)
    pub type_name: String,
    /// Source line where the binding was found
    pub line: u32,
    /// Confidence in this binding (0.0–1.0)
    pub confidence: f32,
    /// How the binding was discovered
    pub source: BindingSource,
}

/// The mechanism that produced a [`TypeBinding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingSource {
    /// Explicit type annotation: `let x: Foo`, `const x: Foo`, `Type x`, `x: Type`
    Annotation,
    /// Constructor call: `new Foo()`, `Foo::new()`, `Foo()`, `Foo{}`
    Constructor,
    /// Self/this receiver in a method body
    SelfThis,
    /// Propagated from another binding via assignment
    Assignment,
    /// Inferred from a function's return type
    ReturnType,
}

// ── Main entry point ──────────────────────────────────────────────────────

/// Extract type bindings from source code using three tiers of analysis.
///
/// Returns a map keyed by `(variable_name, line_number)` to avoid collisions
/// when the same name is rebound on different lines.
///
/// **Tier 0** — Explicit type annotations (confidence 0.95)
/// **Tier 1** — Constructor patterns (confidence 0.90)
/// **Tier 2** — self/this bindings from method parent_name (confidence 1.0)
pub fn extract_bindings(
    source: &str,
    language: Language,
    symbols: &[RawSymbol],
) -> HashMap<(String, u32), TypeBinding> {
    let mut bindings = HashMap::new();

    // Tier 0: annotations
    extract_annotations(source, language, &mut bindings);

    // Tier 1: constructors
    extract_constructors(source, language, symbols, &mut bindings);

    // Tier 2: self/this
    extract_self_bindings(language, symbols, &mut bindings);

    bindings
}

// ── Tier 0: Annotations ──────────────────────────────────────────────────

fn extract_annotations(
    source: &str,
    language: Language,
    bindings: &mut HashMap<(String, u32), TypeBinding>,
) {
    for (line_idx, line) in source.lines().enumerate() {
        let line_num = (line_idx as u32) + 1;
        let trimmed = line.trim();

        let result = match language {
            Language::Rust => extract_rust_annotation(trimmed),
            Language::TypeScript | Language::JavaScript => extract_ts_annotation(trimmed),
            Language::Java | Language::CSharp => extract_java_annotation(trimmed),
            Language::Python => extract_python_annotation(trimmed),
            Language::Go => extract_go_annotation(trimmed),
            _ => None,
        };

        if let Some((var_name, type_name)) = result {
            bindings.insert(
                (var_name, line_num),
                TypeBinding {
                    type_name,
                    line: line_num,
                    confidence: 0.95,
                    source: BindingSource::Annotation,
                },
            );
        }
    }
}

/// Rust: `let var: Type = ...` or `let mut var: Type = ...`
fn extract_rust_annotation(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("let ")?;
    let rest = rest.strip_prefix("mut ").unwrap_or(rest);

    // Find var name (up to ':')
    let colon_pos = rest.find(':')?;
    let var_name = rest[..colon_pos].trim();
    if var_name.is_empty() || !is_identifier(var_name) {
        return None;
    }

    // Extract type after ':'
    let after_colon = rest[colon_pos + 1..].trim();
    let type_name = extract_type_token(after_colon)?;

    Some((var_name.to_string(), type_name))
}

/// TypeScript/JavaScript: `const/let/var name: Type = ...`
fn extract_ts_annotation(line: &str) -> Option<(String, String)> {
    let rest = if let Some(r) = line.strip_prefix("const ") {
        r
    } else if let Some(r) = line.strip_prefix("let ") {
        r
    } else {
        line.strip_prefix("var ")?
    };

    let colon_pos = rest.find(':')?;
    let var_name = rest[..colon_pos].trim();
    if var_name.is_empty() || !is_identifier(var_name) {
        return None;
    }

    let after_colon = rest[colon_pos + 1..].trim();
    let type_name = extract_type_token(after_colon)?;

    Some((var_name.to_string(), type_name))
}

/// Java/C#: `Type name = ...` where Type starts with uppercase.
fn extract_java_annotation(line: &str) -> Option<(String, String)> {
    // Skip common non-type keywords that start with uppercase
    // Also skip lines that start with keywords like return, if, etc.
    let trimmed = line.trim_start();

    // Remove access modifiers
    let rest = strip_modifiers(trimmed);

    // Need at least "Type name"
    let space_pos = rest.find(' ')?;
    let type_candidate = &rest[..space_pos];

    // Type must start with uppercase letter
    if !type_candidate
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        return None;
    }

    if !is_type_identifier(type_candidate) {
        return None;
    }

    // Skip generic type params for the purpose of finding the var name
    let after_type = rest[space_pos..].trim_start();
    let after_generics = skip_generic_params(after_type);

    // Extract variable name (next identifier)
    let var_end = after_generics
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(after_generics.len());
    let var_name = &after_generics[..var_end];

    if var_name.is_empty() || !is_identifier(var_name) {
        return None;
    }

    // Strip generic params from type name for the binding
    let base_type = strip_generics(type_candidate);

    Some((var_name.to_string(), base_type))
}

/// Python: `name: Type = ...`
fn extract_python_annotation(line: &str) -> Option<(String, String)> {
    // Must not start with def/class/import/from/return/if/for/while etc.
    let first_word_end = line
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(line.len());
    let first_word = &line[..first_word_end];

    if matches!(
        first_word,
        "def"
            | "class"
            | "import"
            | "from"
            | "return"
            | "if"
            | "for"
            | "while"
            | "elif"
            | "except"
            | "with"
            | "async"
            | "yield"
    ) {
        return None;
    }

    let colon_pos = line.find(':')?;
    let var_name = line[..colon_pos].trim();

    if var_name.is_empty() || !is_identifier(var_name) {
        return None;
    }

    let after_colon = line[colon_pos + 1..].trim();
    let type_name = extract_type_token(after_colon)?;

    Some((var_name.to_string(), type_name))
}

/// Go: `var name Type`
fn extract_go_annotation(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("var ")?;

    let space_pos = rest.find(' ')?;
    let var_name = &rest[..space_pos];

    if var_name.is_empty() || !is_identifier(var_name) {
        return None;
    }

    let after_name = rest[space_pos..].trim_start();

    // Go type: could start with * (pointer), [] (slice), map[, etc.
    // For simplicity, extract the base type identifier
    let type_str = after_name.split(['=', '\n']).next()?;
    let type_str = type_str.trim();

    if type_str.is_empty() {
        return None;
    }

    // Strip pointer prefix
    let base = type_str.trim_start_matches('*');
    let type_name = extract_type_token(base)?;

    Some((var_name.to_string(), type_name))
}

// ── Tier 1: Constructors ─────────────────────────────────────────────────

fn extract_constructors(
    source: &str,
    language: Language,
    symbols: &[RawSymbol],
    bindings: &mut HashMap<(String, u32), TypeBinding>,
) {
    // Collect known class names from symbols for Python/Kotlin constructor detection
    let class_names: HashSet<&str> = symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Class | SymbolKind::Enum))
        .map(|s| s.name.as_str())
        .collect();

    for (line_idx, line) in source.lines().enumerate() {
        let line_num = (line_idx as u32) + 1;
        let trimmed = line.trim();

        let result = match language {
            Language::TypeScript
            | Language::JavaScript
            | Language::Java
            | Language::CSharp
            | Language::Dart
            | Language::Php => extract_new_constructor(trimmed),
            Language::Rust => extract_rust_constructor(trimmed),
            Language::Python | Language::Kotlin => {
                extract_callable_constructor(trimmed, &class_names)
            }
            Language::Go => extract_go_constructor(trimmed),
            _ => None,
        };

        if let Some((var_name, type_name)) = result {
            let key = (var_name, line_num);
            // Don't overwrite a Tier 0 annotation with a Tier 1 constructor
            bindings.entry(key).or_insert(TypeBinding {
                type_name,
                line: line_num,
                confidence: 0.90,
                source: BindingSource::Constructor,
            });
        }
    }
}

/// `new Foo(...)` pattern for TS/JS/Java/C#/Dart/PHP.
/// Matches lines like `const x = new Foo(...)`, `Type x = new Foo(...)`, etc.
fn extract_new_constructor(line: &str) -> Option<(String, String)> {
    let new_pos = line.find("new ")?;

    // Extract the type name after "new "
    let after_new = &line[new_pos + 4..];
    let type_end = after_new
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.')
        .unwrap_or(after_new.len());
    let type_name = &after_new[..type_end];

    if type_name.is_empty() {
        return None;
    }

    // Use the last segment for qualified names (e.g., `pkg.Foo` → `Foo`)
    let base_type = type_name.rsplit('.').next().unwrap_or(type_name);
    if base_type.is_empty() || !is_type_identifier(base_type) {
        return None;
    }

    // Try to find the variable being assigned
    let before_new = line[..new_pos].trim();
    let var_name = extract_assignment_target(before_new)?;

    Some((var_name, base_type.to_string()))
}

/// Rust: `Foo::new(...)` or `Foo::default(...)`
fn extract_rust_constructor(line: &str) -> Option<(String, String)> {
    // Look for `::new(` or `::default(`
    let constructor_pos = line.find("::new(").or_else(|| line.find("::default("))?;

    // Walk backwards from `::new(` to find the type name
    let before = &line[..constructor_pos];
    let type_start = before
        .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map(|p| {
            // Advance past the matched character (may be multi-byte: em-dash, smart quotes, etc.)
            let ch = before[p..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            p + ch
        })
        .unwrap_or(0);
    let type_name = &before[type_start..];

    if type_name.is_empty() || !is_type_identifier(type_name) {
        return None;
    }

    // Find the variable being assigned.
    // Use the '=' sign from the full line to isolate the LHS.
    // e.g. "let mut name: String = String::new();" → LHS = "let mut name: String"
    let eq_pos = line.find('=')?;
    let lhs = line[..eq_pos].trim();
    // Strip any type annotation after ':'
    let var_part = if let Some(colon_pos) = lhs.find(':') {
        &lhs[..colon_pos]
    } else {
        lhs
    };
    let var_name = extract_assignment_target(var_part.trim())?;

    Some((var_name, type_name.to_string()))
}

/// Python/Kotlin: `Foo(...)` where Foo is a known class from symbols.
fn extract_callable_constructor(
    line: &str,
    class_names: &HashSet<&str>,
) -> Option<(String, String)> {
    // Look for `SomeClass(` pattern
    let paren_pos = line.find('(')?;
    let before_paren = &line[..paren_pos];

    // Extract the last identifier before '('
    let call_start = before_paren
        .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map(|p| {
            let ch = before_paren[p..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            p + ch
        })
        .unwrap_or(0);
    let call_name = &before_paren[call_start..];

    if call_name.is_empty() || !class_names.contains(call_name) {
        return None;
    }

    // Find variable name before the call
    let assign_region = &line[..call_start];
    let var_name = extract_assignment_target(assign_region.trim())?;

    Some((var_name, call_name.to_string()))
}

/// Go: `Foo{}` or `&Foo{}`
fn extract_go_constructor(line: &str) -> Option<(String, String)> {
    // Look for `Type{` or `&Type{`
    let brace_pos = line.find('{')?;
    let before_brace = &line[..brace_pos];
    let before_brace = before_brace.trim_end();

    if before_brace.is_empty() {
        return None;
    }

    // Walk back past optional '&' to find type name
    let type_region = before_brace.trim_end_matches('&');
    let type_start = type_region
        .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.')
        .map(|p| {
            // Advance past the matched character (may be multi-byte: em-dash, smart quotes, etc.)
            let ch = type_region[p..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            p + ch
        })
        .unwrap_or(0);
    let type_name = &type_region[type_start..];

    if type_name.is_empty() || !is_type_identifier(type_name) {
        return None;
    }

    // Find variable being assigned
    let assign_region = &line[..type_start];
    // Also try stripping the '&' if it was directly attached
    let assign_region = assign_region.trim().trim_end_matches('&').trim();
    let var_name = extract_assignment_target(assign_region)?;

    Some((var_name, type_name.to_string()))
}

// ── Tier 2: Self/This ────────────────────────────────────────────────────

pub fn extract_self_bindings(
    language: Language,
    symbols: &[RawSymbol],
    bindings: &mut HashMap<(String, u32), TypeBinding>,
) {
    let self_keyword = match language {
        Language::Rust
        | Language::Python
        | Language::Swift
        | Language::Lua
        | Language::ObjectiveC
        | Language::Pascal => "self",
        Language::TypeScript
        | Language::JavaScript
        | Language::Java
        | Language::CSharp
        | Language::Kotlin
        | Language::Dart
        | Language::Cpp
        | Language::Scala
        | Language::Groovy
        | Language::SystemVerilog => "this",
        Language::Php | Language::PowerShell => "$this",
        Language::Ruby => "self",
        _ => return,
    };

    for sym in symbols {
        if sym.kind != SymbolKind::Method {
            continue;
        }
        if let Some(parent) = &sym.parent_name {
            bindings.insert(
                (self_keyword.to_string(), sym.start_line),
                TypeBinding {
                    type_name: parent.clone(),
                    line: sym.start_line,
                    confidence: 1.0,
                    source: BindingSource::SelfThis,
                },
            );
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Check if a string is a valid identifier (ASCII letters, digits, underscore, starts non-digit).
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check if a string is a valid type identifier (like `is_identifier` but starts with uppercase
/// or is a common type name).
fn is_type_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let base = s.split('<').next().unwrap_or(s);
    let mut chars = base.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Extract the first type token from text after a colon, stopping at `=`, `,`, `)`, `{`, `;`.
/// Handles simple generics like `Vec<String>` by counting angle brackets.
fn extract_type_token(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let mut depth = 0i32;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            '=' | ',' | ')' | '{' | ';' | '\n' if depth == 0 => break,
            ' ' if depth == 0 && i > 0 => break,
            _ => {}
        }
        end = i + c.len_utf8();
    }

    let token = text[..end].trim();
    if token.is_empty() {
        return None;
    }

    // Return the base type name (strip generics for the binding)
    let base = token.split('<').next().unwrap_or(token);
    let base = base.trim_start_matches('*').trim_start_matches('&');

    if base.is_empty() || !is_type_identifier(base) {
        return None;
    }

    Some(base.to_string())
}

/// Strip Java/C# access modifiers from the beginning of a line.
fn strip_modifiers(line: &str) -> &str {
    let mut rest = line;
    for modifier in &[
        "public ",
        "private ",
        "protected ",
        "static ",
        "final ",
        "readonly ",
        "abstract ",
        "volatile ",
        "transient ",
    ] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r;
            // Check for chained modifiers
            return strip_modifiers(rest);
        }
    }
    rest
}

/// Skip generic type parameters like `<String, Integer>`.
fn skip_generic_params(text: &str) -> &str {
    if !text.starts_with('<') {
        return text;
    }
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return text[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    text
}

/// Strip generic parameters from a type name: `HashMap<K,V>` → `HashMap`.
fn strip_generics(s: &str) -> String {
    s.split('<').next().unwrap_or(s).to_string()
}

/// Given the text before an `=` or `new`/constructor, extract the variable name being assigned.
/// E.g. from `const foo =` → `foo`, from `let bar: Type =` → `bar`.
fn extract_assignment_target(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches('=').trim();
    if text.is_empty() {
        return None;
    }

    // For `:=` (Go short assignment)
    let text = text.trim_end_matches(':').trim();

    // Could be `const x`, `let x`, `var x`, `let mut x`, or just `x`
    // Take the last identifier-like token
    let last_token = text
        .rsplit(|c: char| c.is_whitespace() || c == ':')
        .next()?;
    let last_token = last_token.trim();

    if last_token.is_empty() || !is_identifier(last_token) {
        return None;
    }

    Some(last_token.to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::Visibility;

    fn make_symbol(name: &str, kind: SymbolKind, parent: Option<&str>) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind,
            start_line: 1,
            end_line: 10,
            signature: String::new(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: parent.map(|s| s.to_string()),
            scope_chain: None,
        }
    }

    fn make_method(name: &str, parent: &str, start_line: u32) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Method,
            start_line,
            end_line: start_line + 5,
            signature: format!("fn {name}(&self)"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: Some(parent.to_string()),
            scope_chain: None,
        }
    }

    // ── Tier 0: Annotations ──────────────────────────────────────────

    #[test]
    fn rust_annotation_let() {
        let src = "    let count: u32 = 0;\n    let mut name: String = String::new();";
        let bindings = extract_bindings(src, Language::Rust, &[]);
        assert_eq!(bindings.len(), 2);

        let b = &bindings[&("count".to_string(), 1)];
        assert_eq!(b.type_name, "u32");
        assert_eq!(b.confidence, 0.95);
        assert_eq!(b.source, BindingSource::Annotation);

        let b = &bindings[&("name".to_string(), 2)];
        assert_eq!(b.type_name, "String");
    }

    #[test]
    fn rust_constructor_new() {
        let src = "    let map = HashMap::new();";
        let bindings = extract_bindings(src, Language::Rust, &[]);

        let b = &bindings[&("map".to_string(), 1)];
        assert_eq!(b.type_name, "HashMap");
        assert_eq!(b.confidence, 0.90);
        assert_eq!(b.source, BindingSource::Constructor);
    }

    #[test]
    fn typescript_annotation() {
        let src = "const name: string = \"hello\";\nlet items: Array = [];";
        let bindings = extract_bindings(src, Language::TypeScript, &[]);

        let b = &bindings[&("name".to_string(), 1)];
        assert_eq!(b.type_name, "string");
        assert_eq!(b.confidence, 0.95);

        let b = &bindings[&("items".to_string(), 2)];
        assert_eq!(b.type_name, "Array");
    }

    #[test]
    fn typescript_new_constructor() {
        let src = "const svc = new UserService();";
        let bindings = extract_bindings(src, Language::TypeScript, &[]);

        let b = &bindings[&("svc".to_string(), 1)];
        assert_eq!(b.type_name, "UserService");
        assert_eq!(b.confidence, 0.90);
        assert_eq!(b.source, BindingSource::Constructor);
    }

    #[test]
    fn java_annotation() {
        let src = "    HashMap map = new HashMap<>();\n    private final String name = \"foo\";";
        let bindings = extract_bindings(src, Language::Java, &[]);

        let b = &bindings[&("map".to_string(), 1)];
        assert_eq!(b.type_name, "HashMap");
        assert_eq!(b.confidence, 0.95);
        assert_eq!(b.source, BindingSource::Annotation);

        let b = &bindings[&("name".to_string(), 2)];
        assert_eq!(b.type_name, "String");
    }

    #[test]
    fn python_annotation() {
        let src = "count: int = 0\nname: str = \"hello\"";
        let bindings = extract_bindings(src, Language::Python, &[]);

        let b = &bindings[&("count".to_string(), 1)];
        assert_eq!(b.type_name, "int");
        assert_eq!(b.confidence, 0.95);

        let b = &bindings[&("name".to_string(), 2)];
        assert_eq!(b.type_name, "str");
    }

    #[test]
    fn go_annotation_and_constructor() {
        let src = "    var count int\n    svc := &UserService{}";
        let symbols: Vec<RawSymbol> = vec![];
        let bindings = extract_bindings(src, Language::Go, &symbols);

        let b = &bindings[&("count".to_string(), 1)];
        assert_eq!(b.type_name, "int");
        assert_eq!(b.confidence, 0.95);
        assert_eq!(b.source, BindingSource::Annotation);

        let b = &bindings[&("svc".to_string(), 2)];
        assert_eq!(b.type_name, "UserService");
        assert_eq!(b.confidence, 0.90);
        assert_eq!(b.source, BindingSource::Constructor);
    }

    #[test]
    fn self_binding_from_parent() {
        let symbols = vec![
            make_method("process", "OrderService", 10),
            make_method("validate", "OrderService", 20),
            make_symbol("helper", SymbolKind::Function, None),
        ];
        let src = ""; // source doesn't matter for Tier 2
        let bindings = extract_bindings(src, Language::Rust, &symbols);

        // Two methods → two self bindings
        assert_eq!(bindings.len(), 2);

        let b = &bindings[&("self".to_string(), 10)];
        assert_eq!(b.type_name, "OrderService");
        assert_eq!(b.confidence, 1.0);
        assert_eq!(b.source, BindingSource::SelfThis);

        let b = &bindings[&("self".to_string(), 20)];
        assert_eq!(b.type_name, "OrderService");
    }

    #[test]
    fn this_binding_for_typescript() {
        let symbols = vec![make_method("render", "Component", 5)];
        let bindings = extract_bindings("", Language::TypeScript, &symbols);

        let b = &bindings[&("this".to_string(), 5)];
        assert_eq!(b.type_name, "Component");
        assert_eq!(b.confidence, 1.0);
        assert_eq!(b.source, BindingSource::SelfThis);
    }

    #[test]
    fn python_callable_constructor() {
        let symbols = vec![make_symbol("Config", SymbolKind::Class, None)];
        let src = "cfg = Config()";
        let bindings = extract_bindings(src, Language::Python, &symbols);

        let b = &bindings[&("cfg".to_string(), 1)];
        assert_eq!(b.type_name, "Config");
        assert_eq!(b.confidence, 0.90);
        assert_eq!(b.source, BindingSource::Constructor);
    }

    #[test]
    fn annotation_takes_priority_over_constructor() {
        // When both annotation and constructor are detected, annotation wins (Tier 0 first)
        let src = "let map: HashMap = HashMap::new();";
        let bindings = extract_bindings(src, Language::Rust, &[]);

        let b = &bindings[&("map".to_string(), 1)];
        assert_eq!(b.source, BindingSource::Annotation);
        assert_eq!(b.confidence, 0.95);
    }
}
