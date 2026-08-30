; ── C++ symbol extraction ────────────────────────────────────────────
; Based on the official tree-sitter-cpp tags.scm with additions for
; reference extraction (calls, includes).

; Function and method DEFINITIONS are anchored on `function_definition`, not on
; `function_declarator`. The declarator is the signature WITHOUT the body, so
; anchoring the capture there recorded `end_line == start_line` for every C++
; function in the corpus — `setup` in testdata/cpp/simple.cpp read as 31-31
; when it spans 31-36. `queries/c.scm` has always anchored on
; `function_definition` and was always correct; this is that one-node diff.
;
; A zero-height function span is not cosmetic: `read-symbols` returns the
; signature alone, and `find_enclosing_symbol` cannot place a call inside the
; function containing it, so every call in a body was attributed to the nearest
; preceding one-line symbol instead.

; Free function definitions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

; Method definitions via qualified identifier (e.g. Foo::bar)
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier
      name: (identifier) @name))) @definition.method

; Inline method definitions inside a class body
(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @definition.method

; DECLARATIONS with no body — a class's declared interface, and the prototypes
; that make up a header. These are genuinely one line, so they keep the
; declarator anchor. They are matched on their enclosing declaration node
; rather than on `function_declarator` alone so a definition can never match
; both a definition rule and a declaration rule and mint two symbols.
(field_declaration
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @definition.method

(declaration
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

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
