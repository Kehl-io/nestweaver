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

; Constructor: val x = MyClass(...)
; property_declaration positional children: variable_declaration then call_expression
; variable_declaration first child is simple_identifier (the var name)
; call_expression first child is simple_identifier (the class name)
(property_declaration
  (variable_declaration
    (simple_identifier) @ctor.name)
  (call_expression
    (simple_identifier) @ctor.type))
