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

; Type aliases (simple identifier types)
(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (type_identifier))) @definition.type

; Type aliases (slice, map, channel, etc.)
(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (slice_type))) @definition.type

(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (map_type))) @definition.type

(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (function_type))) @definition.type

; Const declarations
(const_declaration
  (const_spec
    name: (identifier) @name)) @definition.const

; Package-level var declarations
(var_declaration
  (var_spec
    name: (identifier) @name)) @definition.variable

; Struct field declarations
(field_declaration
  name: (field_identifier) @name) @definition.field

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (selector_expression
    field: (field_identifier) @name)) @reference.call

; Import specs
(import_spec
  path: (interpreted_string_literal) @name) @reference.import
