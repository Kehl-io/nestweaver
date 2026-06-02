; ── Go type extraction ───────────────────────────────────────────────
; Captures variable-name + type pairs from typed declarations and
; return types.

; var x Type
(var_spec
  name: (identifier) @var.name
  type: (_) @var.type)

; func foo() Type
(function_declaration
  name: (identifier) @return.name
  result: (_) @return.type)

; Method return type: func (r Recv) Foo() Type
(method_declaration
  name: (field_identifier) @return.name
  result: (_) @return.type)

; Parameter type: func foo(x Type)
(parameter_declaration
  name: (identifier) @param.name
  type: (_) @param.type)

; Struct field type: FieldName Type
(field_declaration
  name: (field_identifier) @var.name
  type: (_) @var.type)

; Const with type: const x Type = ...
(const_spec
  name: (identifier) @var.name
  type: (_) @var.type)

; Short variable declaration with composite literal: x := Config{...}
(short_var_declaration
  left: (expression_list
    (identifier) @ctor.name)
  right: (expression_list
    (composite_literal
      type: (type_identifier) @ctor.type)))

; Short variable declaration with qualified composite literal: x := pkg.Config{...}
(short_var_declaration
  left: (expression_list
    (identifier) @ctor.name)
  right: (expression_list
    (composite_literal
      type: (qualified_type) @ctor.type)))
