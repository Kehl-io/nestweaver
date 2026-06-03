; ── Dart type extraction ─────────────────────────────────────────────

; Typed variable: Type name = ...
; type is wrapped in a (type ...) node
(initialized_variable_definition
  (type (type_identifier) @var.type)
  name: (identifier) @var.name)

; Function return type
(function_signature
  return_type: (type (type_identifier) @return.type)
  name: (identifier) @return.name)

; Parameter type
(formal_parameter
  (type (type_identifier) @param.type)
  name: (identifier) @param.name)
