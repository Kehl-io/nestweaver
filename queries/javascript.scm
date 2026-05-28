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

; Export-wrapped arrow functions
(export_statement
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: (arrow_function)))) @definition.function

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

; Class properties (field definitions)
(field_definition
  property: (property_identifier) @name) @definition.property

; Const declarations (non-function values)
(lexical_declaration
  kind: "const"
  (variable_declarator
    name: (identifier) @name
    value: (_) @_val)
  (#not-match? @_val "^(\\(|function|class)")) @definition.const

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

; JSX opening element — component reference
(jsx_opening_element
  name: (identifier) @name) @reference.call

; JSX self-closing element — component reference
(jsx_self_closing_element
  name: (identifier) @name) @reference.call
