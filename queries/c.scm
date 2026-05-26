; Function definitions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

; Struct definitions
(struct_specifier
  name: (type_identifier) @name) @definition.class

; Enum definitions
(enum_specifier
  name: (type_identifier) @name) @definition.class

; Union definitions
(union_specifier
  name: (type_identifier) @name) @definition.class

; Typedef (named via typedef)
(type_definition
  declarator: (type_identifier) @name) @definition.class

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

; #include directives
(preproc_include
  path: [(string_literal) (system_lib_string)] @name) @reference.includes
