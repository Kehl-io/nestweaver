; Function declarations (top-level and within types)
(function_declaration
  name: (simple_identifier) @name) @definition.function

; Class, struct, enum, extension, actor declarations (all use class_declaration node)
(class_declaration
  name: (type_identifier) @name) @definition.class

; Protocol declarations
(protocol_declaration
  name: (type_identifier) @name) @definition.interface

; Type alias declarations
(typealias_declaration
  name: (type_identifier) @name) @definition.type

; Stored properties (var/let declarations inside class/struct)
(class_declaration
  body: (class_body
    (property_declaration
      (pattern
        (simple_identifier) @name)) @definition.property))

; Enum case declarations
(enum_entry
  (simple_identifier) @name) @definition.const

; Import statements — import_declaration > identifier > simple_identifier
(import_declaration
  (identifier
    (simple_identifier) @name)) @reference.import

; Call expressions — direct calls via simple_identifier
(call_expression
  (simple_identifier) @name) @reference.call

; Methods (functions inside class/struct/protocol body)
(class_declaration
  body: (class_body
    (function_declaration
      name: (simple_identifier) @name) @definition.method))

; Inheritance / protocol conformance
(inheritance_specifier
  (user_type
    (type_identifier) @name)) @reference.extends
