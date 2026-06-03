; Function definitions
(function_definition
  (signature
    (call_expression
      (identifier) @name))) @definition.function

(function_definition
  (signature
    (identifier) @name)) @definition.function

; Short-form function definitions: f(x) = x + 1
(assignment
  (call_expression
    (identifier) @name)) @definition.function

; Struct definitions
(struct_definition
  (type_head
    (identifier) @name)) @definition.class

; Module definitions
(module_definition
  name: (identifier) @name) @definition.module

; Macro definitions
(macro_definition
  (signature
    (call_expression
      (identifier) @name))) @definition.function

; Abstract type definitions
(abstract_definition
  (type_head
    (identifier) @name)) @definition.interface

; Import statements
(import_statement
  (import_path
    (identifier) @name)) @reference.import

(import_statement
  (identifier) @name) @reference.import

; Function calls
(call_expression
  (identifier) @name) @reference.call
