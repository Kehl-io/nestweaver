; ── Java type extraction ─────────────────────────────────────────────
; Captures variable-name + type pairs from typed declarations and
; return types.

; Type varName = ...
(local_variable_declaration
  type: (_) @var.type
  declarator: (variable_declarator
    name: (identifier) @var.name))

; Field declarations: private Type field;
(field_declaration
  type: (_) @var.type
  declarator: (variable_declarator
    name: (identifier) @var.name))

; Method return type: public Type foo()
(method_declaration
  type: (_) @return.type
  name: (identifier) @return.name)

; Formal parameter type: void foo(Type x)
(formal_parameter
  type: (_) @param.type
  name: (identifier) @param.name)

; Constructor new expression: new Foo(...)
(object_creation_expression
  type: (type_identifier) @ctor.type)
