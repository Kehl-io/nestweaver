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
  name: (identifier) @name) @definition.class

; Method declarations
(method_declaration
  name: (identifier) @name) @definition.method

; Local function statements
(local_function_statement
  name: (identifier) @name) @definition.function

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
