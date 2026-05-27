; Class definitions
(class_definition
  name: (identifier) @name) @definition.class

; Object definitions (singleton objects)
(object_definition
  name: (identifier) @name) @definition.module

; Trait definitions
(trait_definition
  name: (identifier) @name) @definition.trait

; Function/method definitions
(function_definition
  name: (identifier) @name) @definition.function

; Value definitions (val)
(val_definition
  pattern: (identifier) @name) @definition.const

; Variable definitions (var)
(var_definition
  pattern: (identifier) @name) @definition.variable

; Type definitions (type aliases)
(type_definition
  name: (type_identifier) @name) @definition.type

; Call expressions
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (field_expression
    field: (identifier) @name)) @reference.call

; Import declarations
(import_declaration
  path: (identifier) @name) @reference.import

(import_declaration
  path: (stable_identifier) @name) @reference.import

; Extends (parent classes/traits)
(class_definition
  extend: (extends_clause
    type: (type_identifier) @name)) @reference.extends

(object_definition
  extend: (extends_clause
    type: (type_identifier) @name)) @reference.extends

(trait_definition
  extend: (extends_clause
    type: (type_identifier) @name)) @reference.extends
