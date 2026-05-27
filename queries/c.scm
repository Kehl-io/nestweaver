; Function definitions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

; Struct definitions
(struct_specifier
  name: (type_identifier) @name) @definition.class

; Enum definitions
(enum_specifier
  name: (type_identifier) @name) @definition.enum

; Union definitions
(union_specifier
  name: (type_identifier) @name) @definition.class

; Typedef (type alias)
(type_definition
  declarator: (type_identifier) @name) @definition.type

; #define macros
(preproc_def
  name: (identifier) @name) @definition.macro

; Function-like macros
(preproc_function_def
  name: (identifier) @name) @definition.macro

; Struct/union field declarations
(field_declaration
  declarator: (field_identifier) @name) @definition.field

; Enum values (enumerators)
(enumerator
  name: (identifier) @name) @definition.const

; Global variable declarations
(declaration
  declarator: (init_declarator
    declarator: (identifier) @name)) @definition.variable

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

; #include directives
(preproc_include
  path: [(string_literal) (system_lib_string)] @name) @reference.includes
