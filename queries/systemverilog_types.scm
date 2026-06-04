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
;
; Constructor: MyClass x = new(...) or MyClass x; x = new(...)
; data_declaration children: data_type_or_implicit > data_type > class_type > simple_identifier
; variable_decl_assignment: name: simple_identifier, children include class_new

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

; Constructor: MyClass x = new(...)
; Matches data_declaration where the type is a class_type (not a primitive).
; class_type first child is simple_identifier (class name).
; variable_decl_assignment has name: simple_identifier and class_new child.
(data_declaration
  (data_type_or_implicit
    (data_type
      (class_type
        (simple_identifier) @ctor.type)))
  (list_of_variable_decl_assignments
    (variable_decl_assignment
      name: (simple_identifier) @ctor.name
      (class_new))))
