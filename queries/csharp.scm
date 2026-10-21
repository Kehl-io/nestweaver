; Class declarations
(class_declaration
  name: (identifier) @name) @definition.class

; Struct declarations
(struct_declaration
  name: (identifier) @name) @definition.class

; Interface declarations
(interface_declaration
  name: (identifier) @name) @definition.interface

; Enum declarations
(enum_declaration
  name: (identifier) @name) @definition.enum

; Method declarations
(method_declaration
  name: (identifier) @name) @definition.method

; Constructor declarations
(constructor_declaration
  name: (identifier) @name) @definition.constructor

; Local function statements
(local_function_statement
  name: (identifier) @name) @definition.function

; Property declarations
(property_declaration
  name: (identifier) @name) @definition.property

; Field declarations
(field_declaration
  (variable_declaration
    (variable_declarator
      (identifier) @name))) @definition.property

; Enum member declarations
(enum_member_declaration
  name: (identifier) @name) @definition.const

; Namespace declarations
(namespace_declaration
  name: (identifier) @name) @definition.namespace

(namespace_declaration
  name: (qualified_name) @name) @definition.namespace

; Call expressions
(invocation_expression
  function: (identifier) @name) @reference.call

(invocation_expression
  function: (member_access_expression
    name: (identifier) @name)) @reference.call

; Using directives
(using_directive
  (identifier) @name) @reference.uses

(using_directive
  (qualified_name) @name) @reference.uses

; Base list (extends / implements)
(base_list
  (identifier) @name) @reference.extends

(base_list
  (generic_name
    (identifier) @name)) @reference.extends

; Object construction: `new Foo(...)` is a call of the class, exactly as
; typescript.scm records it (nw-323 defect D). Without it a class that is only
; ever instantiated had no inbound reference at all and `dead-code` reported it.
(object_creation_expression
  type: (identifier) @name) @reference.call

(object_creation_expression
  type: (generic_name
    (identifier) @name)) @reference.call

(object_creation_expression
  type: (qualified_name
    name: (identifier) @name)) @reference.call

(object_creation_expression
  type: (qualified_name
    name: (generic_name
      (identifier) @name))) @reference.call
