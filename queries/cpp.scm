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

; Enum definitions (mapped to class)
(enum_specifier
  name: (type_identifier) @name) @definition.class

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
