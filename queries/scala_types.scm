; ── Scala type extraction ────────────────────────────────────────────

; val/var with type: val x: Type = ...
(val_definition
  pattern: (identifier) @var.name
  type: (type_identifier) @var.type)

(var_definition
  pattern: (identifier) @var.name
  type: (type_identifier) @var.type)

; def return type: def foo(): Type
(function_definition
  name: (identifier) @return.name
  return_type: (type_identifier) @return.type)

; Parameter: def foo(x: Type)
(parameter
  name: (identifier) @param.name
  type: (type_identifier) @param.type)
