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

; Union definitions (mapped to class) -- parity with queries/c.scm:15-16.
; `.h` is dispatched here since nw-352, so a genuine C header's unions must
; still extract or the dispatch move silently loses them.
(union_specifier
  name: (type_identifier) @name) @definition.class

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

; Calls with an explicit template-argument list between the callee name and
; the argument list -- `f<T>(args)`, `ns::f<T>(args)` / `Base::f<T>(args)`,
; and `obj.f<T>(args)` / `ptr->f<T>(args)`. tree-sitter-cpp does not fold the
; template arguments into `identifier`/`qualified_identifier`/`field_expression`
; the way it does for a plain call -- the callee field becomes a distinct
; `template_function` (free/qualified form) or `template_method` (member-access
; form) node, so none of the three patterns above match it and the call is
; dropped entirely rather than resolved. Confirmed missed edge: nw-434,
; `Trees::findAllNodes` -> `_findAllNodes<ParseTree *>(...)` in
; third_party/antlr4_runtime/src/tree/Trees.cpp. Explicit template arguments
; are routine (required whenever the argument can't be deduced), so this is
; not an exotic case -- generic container/visitor/serialization code hits it
; constantly, and a missed CALLS edge here silently understates `impact`,
; `blast-radius` and `flow-trace` too, not just `dead-code`.
;
; Free function / static call, explicit template args: f<T>(...)
(call_expression
  function: (template_function
    name: (identifier) @name)) @reference.call

; Qualified call, explicit template args: ns::f<T>(...) / Base::f<T>(...).
; tree-sitter-cpp parses BOTH the namespace-qualified free function and the
; class-qualified static/inherited method through this same node shape --
; `qualified_identifier`'s `name` field is `template_function` in either case,
; never `template_method` (that variant is reserved for member access below) --
; so one rule covers both.
(call_expression
  function: (qualified_identifier
    name: (template_function
      name: (identifier) @name))) @reference.call

; Member call, explicit template args: obj.f<T>(...) / ptr->f<T>(...).
; `field_expression` covers both `.` and `->` (distinguished by its `operator`
; field, which this rule doesn't need); its `field` is `template_method` when
; the call site supplies explicit template arguments.
(call_expression
  function: (field_expression
    field: (template_method
      name: (field_identifier) @name))) @reference.call

; #include directives.
;
; `@reference.includes`, not `@reference.import`: `#include` is ONE construct
; and `queries/c.scm:48-49` already models it as `@reference.includes`
; (-> EdgeType::Includes / INCLUDES_SYM). Two adjacent languages emitting two
; different reference kinds for the same directive is a graph-shape accident,
; and since nw-352 routed `.h` here it would also have flipped every genuine C
; header's include edges from INCLUDES_SYM to IMPORTS. Both kinds are resolved
; identically by `build_import_graph` (imports.rs:132), so this costs no
; resolution.
(preproc_include
  path: (string_literal) @name) @reference.includes

(preproc_include
  path: (system_lib_string) @name) @reference.includes
