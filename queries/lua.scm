; Global function declarations
(function_declaration
  name: (identifier) @name) @definition.function

; Local function declarations
(function_declaration
  name: (dot_index_expression) @name) @definition.function

; Local function statements
(variable_declaration
  (assignment_statement
    (variable_list
      name: (identifier) @name)
    (expression_list
      value: (function_definition)))) @definition.function

; Method definitions (using colon syntax — table:method())
(function_declaration
  name: (method_index_expression) @name) @definition.method

; Function calls
(function_call
  name: (identifier) @name) @reference.call

(function_call
  name: (dot_index_expression
    field: (identifier) @name)) @reference.call

(function_call
  name: (method_index_expression
    method: (identifier) @name)) @reference.call
