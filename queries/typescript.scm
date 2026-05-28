; Function declarations
(function_declaration
  name: (identifier) @name) @definition.function

(function_signature
  name: (identifier) @name) @definition.function

; Named function expressions assigned to variables
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function

; Export-wrapped arrow functions
(export_statement
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: (arrow_function)))) @definition.function

; Class declarations
(class_declaration
  name: (type_identifier) @name) @definition.class

(abstract_class_declaration
  name: (type_identifier) @name) @definition.class

; Method definitions
(method_definition
  name: (property_identifier) @name) @definition.method

(method_signature
  name: (property_identifier) @name) @definition.method

(abstract_method_signature
  name: (property_identifier) @name) @definition.method

; Interface declarations
(interface_declaration
  name: (type_identifier) @name) @definition.interface

; Type alias declarations
(type_alias_declaration
  name: (type_identifier) @name) @definition.type

; Enum declarations
(enum_declaration
  name: (identifier) @name) @definition.enum

; Class properties (public field definitions)
(public_field_definition
  name: (property_identifier) @name) @definition.property

; Interface property signatures
(property_signature
  name: (property_identifier) @name) @definition.property

; Const declarations (non-function values)
(lexical_declaration
  kind: "const"
  (variable_declarator
    name: (identifier) @name
    value: (_) @_val)
  (#not-match? @_val "^(\\(|function|class)")) @definition.const

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (member_expression
    property: (property_identifier) @name)) @reference.call

; Import statements
(import_statement
  source: (string) @name) @reference.import

; Extends (class heritage)
(class_heritage
  (extends_clause
    value: (identifier) @name)) @reference.extends

; Implements clause
(implements_clause
  (type_identifier) @name) @reference.implements

; ── Type references (USES edges) ────────────────────────────────────
; Parameter type annotation
(required_parameter
  type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Optional parameter type annotation
(optional_parameter
  type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Return type annotation on function declarations
(function_declaration
  return_type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Return type annotation on function signatures
(function_signature
  return_type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Return type annotation on method definitions
(method_definition
  return_type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Return type annotation on method signatures
(method_signature
  return_type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Return type annotation on arrow functions
(arrow_function
  return_type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Variable type annotation
(variable_declarator
  type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; Generic type argument
(generic_type
  (type_identifier) @name) @reference.type_ref

; Property type annotation on public fields
(public_field_definition
  type: (type_annotation
    (type_identifier) @name)) @reference.type_ref

; ── Field access (ACCESSES edges) ───────────────────────────────────
; Property read: obj.field
(member_expression
  property: (property_identifier) @name) @reference.read_access
