; ── Groovy type extraction ───────────────────────────────────────────
; Groovy reuses the Java grammar (tree-sitter-groovy is Java-derived).
; Captures: @var.name+@var.type, @ctor.name+@ctor.type,
;           @return.name+@return.type, @param.name+@param.type

; Type varName = ...
(local_variable_declaration
  type: (_) @var.type
  declarator: (variable_declarator
    name: (identifier) @var.name))

; Method return type: Type methodName()
(method_declaration
  type: (_) @return.type
  name: (identifier) @return.name)

; Parameter: Type param
(formal_parameter
  type: (_) @param.type
  name: (identifier) @param.name)

; Field: Type field
(field_declaration
  type: (_) @var.type
  declarator: (variable_declarator
    name: (identifier) @var.name))

; Constructor with new: Foo x = new Foo()
(local_variable_declaration
  declarator: (variable_declarator
    name: (identifier) @ctor.name
    value: (object_creation_expression
      type: (type_identifier) @ctor.type)))
