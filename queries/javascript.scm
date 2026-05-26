; Function declarations
(function_declaration
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
[
  (class_declaration
    name: (identifier) @name)
  (class
    name: (identifier) @name)
] @definition.class

; Method definitions
(method_definition
  name: (property_identifier) @name) @definition.method

; Call expressions (function calls)
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (member_expression
    property: (property_identifier) @name)) @reference.call

; Import statements (ES modules)
(import_statement
  source: (string) @name) @reference.import

; require() calls
(call_expression
  function: (identifier)
  arguments: (arguments (string) @name)) @reference.import
