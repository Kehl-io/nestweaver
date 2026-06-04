; ── Fortran symbol extraction ─────────────────────────────────────────

; Module definitions
(module
  (module_statement
    (name) @name)) @definition.module

; Program definitions
(program
  (program_statement
    (name) @name)) @definition.module

; Function definitions
(function
  (function_statement
    name: (name) @name)) @definition.function

; Subroutine definitions
(subroutine
  (subroutine_statement
    name: (name) @name)) @definition.function

; Derived type definitions
(derived_type_definition
  (derived_type_statement
    (type_name) @name)) @definition.class

; USE statements (imports)
(use_statement
  (module_name) @name) @reference.import

; Subroutine calls: call name(...)
(subroutine_call
  subroutine: (identifier) @name) @reference.call

; Function calls (call_expression)
(call_expression
  (identifier) @name) @reference.call
