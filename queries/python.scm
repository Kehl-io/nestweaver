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
