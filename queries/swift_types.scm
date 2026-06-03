; ── Swift type extraction ────────────────────────────────────────────

; Typed variable: let/var x: Type = ...
(property_declaration
  (pattern (simple_identifier) @var.name)
  (type_annotation (user_type (type_identifier) @var.type)))

; Function return type: func foo() -> Type
(function_declaration
  name: (simple_identifier) @return.name
  return_type: (user_type (type_identifier) @return.type))

; Parameter: func foo(x: Type)
(parameter
  (simple_identifier) @param.name
  (user_type (type_identifier) @param.type))

; Constructor: let x = MyClass(...)
; property_declaration.name is pattern > simple_identifier
; property_declaration.value can be call_expression whose first child is simple_identifier
(property_declaration
  name: (pattern (simple_identifier) @ctor.name)
  value: (call_expression
    (simple_identifier) @ctor.type))

; Constructor via constructor_expression: let x = MyClass()
; Swift uses constructor_expression when the type is explicit without ()
; constructed_type: user_type > type_identifier
(property_declaration
  name: (pattern (simple_identifier) @ctor.name)
  value: (constructor_expression
    constructed_type: (user_type (type_identifier) @ctor.type)))
