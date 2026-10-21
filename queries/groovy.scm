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

; Object construction: `new Foo(...)` is a call of the class, exactly as
; typescript.scm records it (nw-323 defect D). Without it a class that is only
; ever instantiated had no inbound reference at all and `dead-code` reported it.
(object_creation_expression
  type: (type_identifier) @name) @reference.call

(object_creation_expression
  type: (generic_type
    (type_identifier) @name)) @reference.call

(object_creation_expression
  type: (scoped_type_identifier
    (type_identifier) @name .)) @reference.call
