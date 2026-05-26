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

; Enum definitions (mapped to class)
(enum_item
  name: (type_identifier) @name) @definition.class

; Trait definitions (mapped to interface)
(trait_item
  name: (type_identifier) @name) @definition.interface

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
