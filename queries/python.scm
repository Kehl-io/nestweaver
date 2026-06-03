; Method definitions (functions inside a class body — must come before the general function pattern)
(class_definition
  body: (block
    (function_definition
      name: (identifier) @name) @definition.method))

; Function definitions
(function_definition
  name: (identifier) @name) @definition.function

; Class definitions
(class_definition
  name: (identifier) @name) @definition.class

; Module-level variable assignments (NAME = value at top level)
(module
  (expression_statement
    (assignment
      left: (identifier) @name))) @definition.variable

; Class-level attribute assignments (NAME = value inside class body)
(class_definition
  body: (block
    (expression_statement
      (assignment
        left: (identifier) @name)))) @definition.property

; Instance attributes set via self.x = ... inside methods
(class_definition
  body: (block
    (function_definition
      body: (block
        (expression_statement
          (assignment
            left: (attribute
              object: (identifier) @_self
              attribute: (identifier) @name))))))) @definition.property

; Call expressions
(call
  function: (identifier) @name) @reference.call

(call
  function: (attribute
    attribute: (identifier) @name)) @reference.call

; Import from statements
(import_from_statement
  module_name: (dotted_name) @name) @reference.import

(import_from_statement
  module_name: (relative_import) @name) @reference.import

; Import statements
(import_statement
  name: (dotted_name) @name) @reference.import

; Extends (superclasses)
(class_definition
  superclasses: (argument_list
    (identifier) @name)) @reference.extends

; Decorator references
(decorator
  (identifier) @name) @reference.call

; ── Type references (USES edges) ────────────────────────────────────
; Parameter type annotation: def foo(x: MyType)
(typed_parameter
  type: (type) @name) @reference.type_ref

; Return type annotation: def foo() -> MyType:
(function_definition
  return_type: (type) @name) @reference.type_ref

; Variable type annotation: x: MyType = ...
(type
  (identifier) @name) @reference.type_ref

; ── Field access (ACCESSES edges) ───────────────────────────────────
; Attribute read: obj.field
(attribute
  attribute: (identifier) @name) @reference.read_access
