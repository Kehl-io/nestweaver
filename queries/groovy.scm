; Class declarations
(class_declaration
  name: (identifier) @name) @definition.class

; Interface declarations
(interface_declaration
  name: (identifier) @name) @definition.interface

; Enum declarations
(enum_declaration
  name: (identifier) @name) @definition.enum

; Method declarations (inside class bodies)
(method_declaration
  name: (identifier) @name) @definition.method

; Constructor declarations
(constructor_declaration
  name: (identifier) @name) @definition.constructor

; Function definitions (Groovy closures assigned to names)
(function_definition
  name: (identifier) @name) @definition.function

; Field declarations
(field_declaration
  declarator: (variable_declarator
    name: (identifier) @name)) @definition.property

; Enum constants
(enum_constant
  name: (identifier) @name) @definition.const

; Method invocations (calls)
(method_invocation
  name: (identifier) @name
  arguments: (argument_list)) @reference.call

; Import declarations
(import_declaration
  (scoped_identifier) @name) @reference.import

(import_declaration
  (identifier) @name) @reference.import

; Extends (superclass)
(superclass
  (type_identifier) @name) @reference.extends

; Implements (super_interfaces)
(super_interfaces
  (type_list
    (type_identifier) @name)) @reference.implements

; Annotation references
(marker_annotation
  name: (identifier) @name) @reference.call

(annotation
  name: (identifier) @name) @reference.call

; Type references
(method_declaration
  type: (type_identifier) @name) @reference.type_ref

(formal_parameter
  type: (type_identifier) @name) @reference.type_ref

(field_declaration
  type: (type_identifier) @name) @reference.type_ref

(local_variable_declaration
  type: (type_identifier) @name) @reference.type_ref
