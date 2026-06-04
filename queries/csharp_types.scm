; ── C# type extraction ───────────────────────────────────────────────

; Local variable: Type name = ...
(variable_declaration
  type: (identifier) @var.type
  (variable_declarator
    name: (identifier) @var.name))

; Local variable with predefined type: string s = ...
(variable_declaration
  type: (predefined_type) @var.type
  (variable_declarator
    name: (identifier) @var.name))

; Method return type (identifier type)
(method_declaration
  returns: (identifier) @return.type
  name: (identifier) @return.name)

; Method return type (predefined like int, string)
(method_declaration
  returns: (predefined_type) @return.type
  name: (identifier) @return.name)

; Parameter with identifier type
(parameter
  type: (identifier) @param.type
  name: (identifier) @param.name)

; Parameter with predefined type
(parameter
  type: (predefined_type) @param.type
  name: (identifier) @param.name)

; new expression (standalone, no variable binding — kept for other code paths)
(object_creation_expression
  type: (identifier) @ctor.type)

; Variable with new: var x = new Foo() or Type x = new Foo()
; variable_declarator has name: identifier and an expression child (object_creation_expression)
(variable_declaration
  (variable_declarator
    name: (identifier) @ctor.name
    (object_creation_expression
      type: (identifier) @ctor.type)))
