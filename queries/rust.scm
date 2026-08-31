; ── Rust symbol extraction ───────────────────────────────────────────
; Based on the official tree-sitter-rust tags.scm with additions for
; reference extraction (calls, use declarations, trait impls).

; Method definitions (functions inside impl/trait blocks)
(declaration_list
  (function_item
    name: (identifier) @name) @definition.method)

; Free function definitions
(function_item
  name: (identifier) @name) @definition.function

; Struct definitions (mapped to class, matching Go precedent)
(struct_item
  name: (type_identifier) @name) @definition.class

; Enum definitions
(enum_item
  name: (type_identifier) @name) @definition.enum

; Trait definitions (mapped to interface)
(trait_item
  name: (type_identifier) @name) @definition.interface

; Const items
(const_item
  name: (identifier) @name) @definition.const

; Static items
(static_item
  name: (identifier) @name) @definition.static

; Type aliases
(type_item
  name: (type_identifier) @name) @definition.type

; Module declarations
(mod_item
  name: (identifier) @name) @definition.module

; Macro definitions
(macro_definition
  name: (identifier) @name) @definition.macro

; Struct field declarations
(field_declaration
  name: (field_identifier) @name) @definition.field

; Impl blocks
(impl_item
  type: (type_identifier) @name) @definition.impl

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @reference.call

(call_expression
  function: (scoped_identifier
    name: (identifier) @name)) @reference.call

; Macro invocations as calls
(macro_invocation
  macro: (identifier) @name) @reference.call

; Use declarations as imports. The whole declaration is captured and expanded
; in parse.rs (expand_rust_use_imports) so list forms (`use a::{b, c};`),
; wildcards (`use a::*;`) and aliases (`use a::b as c;`) all yield import
; references — tree-sitter patterns cannot express the per-leaf expansion.
(use_declaration) @reference.rust_use

; Trait implementations as extends
(impl_item
  trait: (type_identifier) @name) @reference.extends

; ── Type references (USES edges) ────────────────────────────────────
; Parameter type: fn foo(x: MyType)
(parameter
  type: (type_identifier) @name) @reference.type_ref

; Return type: fn foo() -> MyType
(function_item
  return_type: (type_identifier) @name) @reference.type_ref

; Struct field type: field: MyType
(field_declaration
  type: (type_identifier) @name) @reference.type_ref

; Let binding type: let x: MyType = ...
(let_declaration
  type: (type_identifier) @name) @reference.type_ref

; Generic type argument: Vec<MyType>
(generic_type
  type: (type_identifier) @name) @reference.type_ref

; Reference type: &MyType
(reference_type
  type: (type_identifier) @name) @reference.type_ref

; Struct literal: `MyType { .. }` CONSTRUCTS the type, which is the strongest
; possible use of it, yet every rule above matches only a type ANNOTATION. A
; struct that is only ever built and never annotated therefore had no inbound
; edge at all -- `MaintenanceHandle`, `ChildGuard`, `RuntimeDirFixture` and
; `PreparedRestart` were all reported as unreachable while being constructed in
; their own file (nw-291 follow-up).
(struct_expression
  name: (type_identifier) @name) @reference.type_ref

; Associated-item path: `MyType::new()`, `MyType::CONST`. The .scm already
; captures the trailing `name:` as a call; the leading `path:` is a use of the
; TYPE and was captured nowhere. Gated on an upper-case initial so a module
; path -- `std::fs`, `tools::dispatch` -- is not turned into a type reference
; and cannot bind to a same-named symbol somewhere else in the graph.
((scoped_identifier
   path: (identifier) @name) @reference.type_ref
 (#match? @name "^[A-Z]"))

; ── Field access (ACCESSES edges) ───────────────────────────────────
; Field expression: obj.field
(field_expression
  field: (field_identifier) @name) @reference.read_access

; ── Attribute-string references (nw-349, cause 3) ───────────────────────
; `#[serde(default = "f")]` names a function as a STRING, so no
; `call_expression` rule above can see it and `f` has in-degree 0. Measured
; in-tree before this rule: 97 serde attribute sites, 31 distinct named
; functions, 12 of which have NO ordinary call site anywhere in the workspace —
; twelve guaranteed `dead-code` false positives on this repository's own source.
;
; The key allow-list is the counterweight, and without it this rule FABRICATES
; edges. `#[serde(rename = "camelCase")]`, `#[error("Database not found: {path}")]`
; and `#[diagnostic(code(nestweaver::db_not_found))]` all carry string literals
; that name nothing, and a blanket `(string_literal)` inside any attribute would
; mint a reference for every one of them. Only the spellings that are DOCUMENTED
; to name a path are matched.
;
; A path-qualified value (`with = "mod::f"`) resolves to nothing and becomes
; `unresolved:{name}` at confidence 0, which is harmless — a miss, not a lie.
((attribute
   (token_tree
     (identifier) @_key
     .
     "="
     .
     (string_literal) @name)) @reference.call
 (#match? @_key "^(default|with|serialize_with|deserialize_with|skip_serializing_if|deserialize_state|value_parser)$"))

; `default` is a CONTEXTUAL KEYWORD in Rust (`default impl`), so tree-sitter
; tokenises it as an anonymous node rather than an `identifier` and the
; allow-listed pattern above cannot see it. It is also the single most common
; spelling of this construct — 12 of the 12 measured false positives are
; `default = "..."` — so a rule that silently missed it would have closed almost
; nothing while appearing to work.
((attribute
   (token_tree
     "default"
     .
     "="
     .
     (string_literal) @name)) @reference.call)
