; ── TypeScript type extraction ───────────────────────────────────────
; Captures variable-name + type pairs from typed declarations,
; constructor calls, and return types.

; const/let/var x: Type = ...
(variable_declarator
  name: (identifier) @var.name
  type: (type_annotation (_) @var.type))

; const x = new Foo(...)
(variable_declarator
  name: (identifier) @ctor.name
  value: (new_expression
    constructor: (identifier) @ctor.type))

; Function return type: function foo(): Type
(function_declaration
  name: (identifier) @return.name
  return_type: (type_annotation (_) @return.type))

; Function signature return type
(function_signature
  name: (identifier) @return.name
  return_type: (type_annotation (_) @return.type))

; Method return type
(method_definition
  name: (property_identifier) @return.name
  return_type: (type_annotation (_) @return.type))

; Method signature return type
(method_signature
  name: (property_identifier) @return.name
  return_type: (type_annotation (_) @return.type))

; Required parameter type
(required_parameter
  pattern: (identifier) @param.name
  type: (type_annotation (_) @param.type))

; Optional parameter type
(optional_parameter
  pattern: (identifier) @param.name
  type: (type_annotation (_) @param.type))

; Public field definition type
(public_field_definition
  name: (property_identifier) @var.name
  type: (type_annotation (_) @var.type))

; Class property with new: class Foo { prop = new Bar() }
(public_field_definition
  name: (property_identifier) @ctor.name
  value: (new_expression
    constructor: (identifier) @ctor.type))
