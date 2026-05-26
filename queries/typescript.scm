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
