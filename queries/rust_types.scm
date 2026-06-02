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

; Struct expression constructor: let config = Config { host: "localhost" }
(let_declaration
  pattern: (identifier) @ctor.name
  value: (struct_expression
    name: (type_identifier) @ctor.type))

; Scoped struct expression constructor: let v = std::collections::HashMap { }
(let_declaration
  pattern: (identifier) @ctor.name
  value: (struct_expression
    name: (scoped_type_identifier) @ctor.type))

; Tuple struct destructuring: let Point(x, y) = value
; Captures type as both ctor.name and ctor.type so the parser can form a binding.
(let_declaration
  pattern: (tuple_struct_pattern
    type: (identifier) @ctor.name @ctor.type))

; Struct pattern destructuring: let Foo { x, y } = value
; Captures type as both ctor.name and ctor.type so the parser can form a binding.
(let_declaration
  pattern: (struct_pattern
    type: (type_identifier) @ctor.name @ctor.type))

; Constructor-style call with variable: let x = Foo::new(...)
; Captures both the variable name and the type from the path
(let_declaration
  pattern: (identifier) @ctor.name
  value: (call_expression
    function: (scoped_identifier
      path: (identifier) @ctor.type)))

; Scoped constructor: let x = foo::bar::Baz::from(...)
(let_declaration
  pattern: (identifier) @ctor.name
  value: (call_expression
    function: (scoped_identifier
      path: (scoped_identifier) @ctor.type)))
