; Function definitions
(function_definition
  name: (name) @name) @definition.function

; Class declarations
(class_declaration
  name: (name) @name) @definition.class

; Interface declarations
(interface_declaration
  name: (name) @name) @definition.interface

; Trait declarations
(trait_declaration
  name: (name) @name) @definition.trait

; Method declarations
(method_declaration
  name: (name) @name) @definition.method

; Enum declarations
(enum_declaration
  name: (name) @name) @definition.class

; Function call expressions
(function_call_expression
  function: (name) @name) @reference.call

; Member call expressions
(member_call_expression
  name: (name) @name) @reference.call

; Scoped call expressions (static methods)
(scoped_call_expression
  name: (name) @name) @reference.call

; Namespace use declarations (top-level use statements)
(namespace_use_declaration
  (namespace_use_clause
    (qualified_name) @name)) @reference.uses

; Base class (extends)
(base_clause
  (name) @name) @reference.extends

(base_clause
  (qualified_name) @name) @reference.extends

; Implements
(class_interface_clause
  (name) @name) @reference.implements

(class_interface_clause
  (qualified_name) @name) @reference.implements
