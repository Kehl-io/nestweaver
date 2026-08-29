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
