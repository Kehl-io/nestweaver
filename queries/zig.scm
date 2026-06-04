; Function declarations
(function_declaration
  name: (identifier) @name) @definition.function

; Struct declarations (const Name = struct { ... })
(variable_declaration
  (identifier) @name
  (struct_declaration)) @definition.class

; Enum declarations (const Name = enum { ... })
(variable_declaration
  (identifier) @name
  (enum_declaration)) @definition.enum

; Union declarations (const Name = union { ... })
(variable_declaration
  (identifier) @name
  (union_declaration)) @definition.class

; Error set declarations (const Name = error { ... })
(variable_declaration
  (identifier) @name
  (error_set_declaration)) @definition.enum

; Test declarations
(test_declaration
  (string) @name) @definition.function

; @import("module") — capture the string argument as an import reference
(builtin_function
  (builtin_identifier) @_builtin_name
  (arguments
    (string
      (string_content) @name))
  (#eq? @_builtin_name "@import")) @reference.import

; Regular function calls
(call_expression
  function: (identifier) @name) @reference.call

; Field expression calls (obj.method())
(call_expression
  function: (field_expression
    member: (identifier) @name)) @reference.call

; Non-import builtin function calls (@intCast, @as, etc.)
(builtin_function
  (builtin_identifier) @name
  (#not-eq? @name "@import")) @reference.call
