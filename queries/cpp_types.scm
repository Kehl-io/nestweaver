; ── C++ type extraction ──────────────────────────────────────────────

; Typed local variable with named type: MyClass obj = ...
(declaration
  type: (type_identifier) @var.type
  declarator: (init_declarator
    declarator: (identifier) @var.name))

; Typed local variable with primitive type: int count = ...
(declaration
  type: (primitive_type) @var.type
  declarator: (init_declarator
    declarator: (identifier) @var.name))

; Pointer variable with named type: MyClass* ptr = ...
(declaration
  type: (type_identifier) @var.type
  declarator: (init_declarator
    declarator: (pointer_declarator
      declarator: (identifier) @var.name)))

; Function return type with named type
(function_definition
  type: (type_identifier) @return.type
  declarator: (function_declarator
    declarator: (identifier) @return.name))

; Function return type with primitive type
(function_definition
  type: (primitive_type) @return.type
  declarator: (function_declarator
    declarator: (identifier) @return.name))

; Parameter with named type
(parameter_declaration
  type: (type_identifier) @param.type
  declarator: (identifier) @param.name)

; Parameter with primitive type
(parameter_declaration
  type: (primitive_type) @param.type
  declarator: (identifier) @param.name)

; new expression in declaration
(declaration
  declarator: (init_declarator
    declarator: (identifier) @ctor.name
    value: (new_expression
      type: (type_identifier) @ctor.type)))
