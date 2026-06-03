; ── Kotlin type extraction ───────────────────────────────────────────

; val/var with type: val x: Type = ...
; variable_declaration contains both simple_identifier and user_type as children
(variable_declaration
  (simple_identifier) @var.name
  (user_type
    (type_identifier) @var.type))

; Function return type: fun foo(): Type
(function_declaration
  (simple_identifier) @return.name
  (user_type
    (type_identifier) @return.type))

; Parameter: fun foo(x: Type)
(parameter
  (simple_identifier) @param.name
  (user_type
    (type_identifier) @param.type))
