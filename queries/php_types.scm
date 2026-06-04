; ── PHP type extraction ──────────────────────────────────────────────

; Function return type with named class: function foo(): MyClass
(function_definition
  name: (name) @return.name
  return_type: (named_type (name) @return.type))

; Function return type with primitive: function foo(): string
(function_definition
  name: (name) @return.name
  return_type: (primitive_type) @return.type)

; Method return type with named class
(method_declaration
  name: (name) @return.name
  return_type: (named_type (name) @return.type))

; Method return type with primitive
(method_declaration
  name: (name) @return.name
  return_type: (primitive_type) @return.type)

; Typed parameter with named class: function foo(MyClass $param)
(simple_parameter
  type: (named_type (name) @param.type)
  name: (variable_name (name) @param.name))

; new expression
(object_creation_expression
  (name) @ctor.type)
