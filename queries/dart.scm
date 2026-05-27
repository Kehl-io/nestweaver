; Class declarations
(class_declaration
  name: (identifier) @name) @definition.class

; Mixin declarations (mapped to trait)
(mixin_declaration
  name: (identifier) @name) @definition.trait

; Enum declarations
(enum_declaration
  name: (identifier) @name) @definition.enum

; Top-level function declarations
(function_declaration
  signature: (function_signature
    name: (identifier) @name)) @definition.function

; Method declarations (inside class body)
(method_signature
  (function_signature
    name: (identifier) @name)) @definition.method

; Import directives — library_import > import_specification > configurable_uri > uri > string_literal
(library_import
  (import_specification
    uri: (configurable_uri
      (uri
        (string_literal) @name)))) @reference.import

; Call expressions — direct function calls
(call_expression
  function: (identifier) @name) @reference.call

; Call expressions — method calls (member access)
(call_expression
  function: (member_expression
    property: (identifier) @name)) @reference.call

; Superclass (extends)
(superclass
  type: (type
    (type_identifier) @name)) @reference.extends

; Implements
(interfaces
  (type
    (type_identifier) @name)) @reference.implements

; Mixins (with)
(mixins
  (type
    (type_identifier) @name)) @reference.implements
