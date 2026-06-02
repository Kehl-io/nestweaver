; ── Rust type extraction ─────────────────────────────────────────────
; Captures variable-name + type pairs from typed declarations,
; constructor calls, and return types.

; let x: Type = ...
(let_declaration
  pattern: (identifier) @var.name
  type: (_) @var.type)

; Function return type: fn foo() -> Type
(function_item
  name: (identifier) @return.name
  return_type: (_) @return.type)

; Struct field with type: field: Type
(field_declaration
  name: (field_identifier) @var.name
  type: (_) @var.type)

; Parameter type: fn foo(x: Type)
(parameter
  pattern: (identifier) @param.name
  type: (_) @param.type)

; Const item: const X: Type = ...
(const_item
  name: (identifier) @var.name
  type: (_) @var.type)

; Static item: static X: Type = ...
(static_item
  name: (identifier) @var.name
  type: (_) @var.type)

; Constructor-style call: Foo::new(...)
(call_expression
  function: (scoped_identifier
    path: (identifier) @ctor.type
    name: (identifier) @_method)
  (#eq? @_method "new")) @ctor.call
