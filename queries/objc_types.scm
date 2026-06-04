; ── Objective-C type extraction ──────────────────────────────────────
; ObjC inherits from the C grammar (tree-sitter-objc extends tree-sitter-c).
; C-style declarations use named fields (type:, declarator:).
; ObjC method declarations/definitions do NOT use named fields for return
; type — they use positional children (method_type), so we can't capture
; those with field syntax.

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

; C-style parameter type
(parameter_declaration
  type: (type_identifier) @param.type
  declarator: (identifier) @param.name)

; C-style parameter type (primitive)
(parameter_declaration
  type: (primitive_type) @param.type
  declarator: (identifier) @param.name)

; Struct/class field type
(field_declaration
  type: (type_identifier) @var.type
  declarator: (field_identifier) @var.name)
