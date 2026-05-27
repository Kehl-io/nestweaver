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
