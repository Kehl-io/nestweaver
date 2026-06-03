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
  name: (name) @name) @definition.enum

; Class property declarations
(property_declaration
  (property_element
    (variable_name) @name)) @definition.property

; Class constant declarations
(const_declaration
  (const_element
    (name) @name)) @definition.const

; Enum case declarations
(enum_case
  name: (name) @name) @definition.const

; Instance property: $this->prop = value
(assignment_expression
  left: (member_access_expression
    object: (variable_name) @_this
    name: (name) @name)
  (#eq? @_this "$this")) @definition.property

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
