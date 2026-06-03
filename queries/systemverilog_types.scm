; ── SystemVerilog type extraction ────────────────────────────────────
; SystemVerilog has typed declarations like `int count;` and class properties.
;
; data_declaration: no named fields; children = [data_type_or_implicit, list_of_variable_decl_assignments]
; variable_decl_assignment: field name = simple_identifier | escaped_identifier
; function_body_declaration: field name; children include data_type_or_void
; tf_port_item: field name; children include data_type_or_implicit
;
; The grammar uses deeply nested structures without named type/declarator
; fields on the containing data_declaration. We match the inner nodes.

; Variable declaration: data_type followed by variable name
; e.g., int count; logic [7:0] data;
(data_declaration
  (data_type_or_implicit
    (data_type
      (integer_atom_type) @var.type))
  (list_of_variable_decl_assignments
    (variable_decl_assignment
      name: (simple_identifier) @var.name)))

; Function return type: function int get_count();
(function_body_declaration
  (data_type_or_void
    (data_type
      (integer_atom_type) @return.type))
  name: (simple_identifier) @return.name)

; Function parameter: function void foo(int bar);
(tf_port_item
  (data_type_or_implicit
    (data_type
      (integer_atom_type) @param.type))
  name: (simple_identifier) @param.name)
