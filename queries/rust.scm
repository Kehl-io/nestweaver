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

; Use declarations as imports
(use_declaration
  argument: (scoped_identifier) @name) @reference.import

(use_declaration
  argument: (identifier) @name) @reference.import

(use_declaration
  argument: (use_as_clause
    path: (scoped_identifier) @name)) @reference.import

; Trait implementations as extends
(impl_item
  trait: (type_identifier) @name) @reference.extends
