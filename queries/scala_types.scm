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

; Constructor: val x = new MyClass(...)
; instance_expression children include type_identifier (the class name)
(val_definition
  pattern: (identifier) @ctor.name
  value: (instance_expression
    (type_identifier) @ctor.type))

(var_definition
  pattern: (identifier) @ctor.name
  value: (instance_expression
    (type_identifier) @ctor.type))

; Apply-style constructor: val x = MyClass(...)
; call_expression.function is identifier (the class name)
(val_definition
  pattern: (identifier) @ctor.name
  value: (call_expression
    function: (identifier) @ctor.type))

(var_definition
  pattern: (identifier) @ctor.name
  value: (call_expression
    function: (identifier) @ctor.type))
