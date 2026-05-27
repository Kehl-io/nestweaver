; Function declarations (top-level)
(function_declaration
  (simple_identifier) @name) @definition.function

; Class declarations (also matches interface declarations, which use `interface` keyword)
(class_declaration
  (type_identifier) @name) @definition.class

; Object declarations (companion objects and top-level objects)
(object_declaration
  (type_identifier) @name) @definition.module

; Methods (functions inside class body)
(class_body
  (function_declaration
    (simple_identifier) @name) @definition.method)

; Property declarations (val/var)
(property_declaration
  (variable_declaration
    (simple_identifier) @name)) @definition.property

; Enum entries
(enum_entry
  (simple_identifier) @name) @definition.const

; Call expressions — direct: greet(...)
(call_expression
  (simple_identifier) @name) @reference.call

; Call expressions — navigation: greeter.greet(...) or Helper.assist()
(call_expression
  (navigation_expression
    (navigation_suffix
      (simple_identifier) @name))) @reference.call

; Import headers
(import_header
  (identifier) @name) @reference.import

; Delegation specifiers (extends / implements): class Foo : Bar
(delegation_specifier
  (user_type
    (type_identifier) @name)) @reference.extends

; Constructor invocation delegation specifiers: class Foo : Bar(...)
(delegation_specifier
  (constructor_invocation
    (user_type
      (type_identifier) @name))) @reference.extends
