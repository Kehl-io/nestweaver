; ── C++ symbol extraction ────────────────────────────────────────────
; Based on the official tree-sitter-cpp tags.scm with additions for
; reference extraction (calls, includes).

; Free function definitions
(function_declarator
  declarator: (identifier) @name) @definition.function

; Method definitions via qualified identifier (e.g. Foo::bar)
(function_declarator
  declarator: (qualified_identifier
    name: (identifier) @name)) @definition.method

(function_declarator
  declarator: (field_identifier) @name) @definition.method

; Class definitions
(class_specifier
  name: (type_identifier) @name) @definition.class

; Struct definitions (mapped to class)
(struct_specifier
  name: (type_identifier) @name
  body: (_)) @definition.class

; Enum definitions
(enum_specifier
  name: (type_identifier) @name) @definition.enum

; #define macros
(preproc_def
  name: (identifier) @name) @definition.macro

; Function-like macros
(preproc_function_def
  name: (identifier) @name) @definition.macro

; Struct/union field declarations
(field_declaration
  declarator: (field_identifier) @name) @definition.field

; Type aliases (typedef and using)
(type_definition
  declarator: (type_identifier) @name) @definition.type

(alias_declaration
  name: (type_identifier) @name) @definition.type

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

(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @reference.call

(call_expression
  function: (qualified_identifier
    name: (identifier) @name)) @reference.call

; #include directives
(preproc_include
  path: (string_literal) @name) @reference.import

(preproc_include
  path: (system_lib_string) @name) @reference.import
