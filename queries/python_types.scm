; ── Python type extraction ───────────────────────────────────────────
; Captures variable-name + type pairs from type annotations and
; return types.

; x: Type = ...
(assignment
  left: (identifier) @var.name
  type: (type (_) @var.type))

; Function return type: def foo() -> Type:
(function_definition
  name: (identifier) @return.name
  return_type: (type (_) @return.type))

; Parameter type annotation: def foo(x: Type)
(typed_parameter
  (identifier) @param.name
  type: (type (_) @param.type))
