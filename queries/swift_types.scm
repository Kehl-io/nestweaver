; ── Swift type extraction ────────────────────────────────────────────

; Typed variable: let/var x: Type = ...
(property_declaration
  (pattern (simple_identifier) @var.name)
  (type_annotation (user_type (type_identifier) @var.type)))

; Function return type: func foo() -> Type
(function_declaration
  name: (simple_identifier) @return.name
  name: (user_type (type_identifier) @return.type))

; Parameter: func foo(x: Type)
(parameter
  (simple_identifier) @param.name
  (user_type (type_identifier) @param.type))
