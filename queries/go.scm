; Function declarations
(function_declaration
  name: (identifier) @name) @definition.function

; Method declarations
(method_declaration
  name: (field_identifier) @name) @definition.method

; Struct type specs (mapped to class)
(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (struct_type))) @definition.class

; Interface type specs
(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (interface_type))) @definition.interface

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (selector_expression
    field: (field_identifier) @name)) @reference.call

; Import specs
(import_spec
  path: (interpreted_string_literal) @name) @reference.import
