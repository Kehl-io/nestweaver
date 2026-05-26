; Class declarations
(class_declaration
  name: (identifier) @name) @definition.class

; Interface declarations
(interface_declaration
  name: (identifier) @name) @definition.interface

; Method declarations
(method_declaration
  name: (identifier) @name) @definition.method

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
