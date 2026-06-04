; ── C type extraction ────────────────────────────────────────────────

; Typed variable: Type name = ...
(declaration
  type: (type_identifier) @var.type
  declarator: (init_declarator
    declarator: (identifier) @var.name))

; Primitive typed variable: int name = ...
(declaration
  type: (primitive_type) @var.type
  declarator: (init_declarator
    declarator: (identifier) @var.name))

; Pointer variable: Type* name
(declaration
  type: (type_identifier) @var.type
  declarator: (init_declarator
    declarator: (pointer_declarator
      declarator: (identifier) @var.name)))

; Function return type
(function_definition
  type: (type_identifier) @return.type
  declarator: (function_declarator
    declarator: (identifier) @return.name))

; Function return type (primitive)
(function_definition
  type: (primitive_type) @return.type
  declarator: (function_declarator
    declarator: (identifier) @return.name))

; Parameter type
(parameter_declaration
  type: (type_identifier) @param.type
  declarator: (identifier) @param.name)

; Parameter type (primitive)
(parameter_declaration
  type: (primitive_type) @param.type
  declarator: (identifier) @param.name)

; Struct field type
(field_declaration
  type: (type_identifier) @var.type
  declarator: (field_identifier) @var.name)
