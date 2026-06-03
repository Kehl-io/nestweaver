; ── SystemVerilog symbol extraction ───────────────────────────────────

; Module declarations
(module_declaration
  (module_ansi_header
    name: (simple_identifier) @name)) @definition.module

(module_declaration
  (module_nonansi_header
    name: (simple_identifier) @name)) @definition.module

; Interface declarations
(interface_declaration
  (interface_ansi_header
    name: (simple_identifier) @name)) @definition.interface

; Class declarations
(class_declaration
  name: (simple_identifier) @name) @definition.class

; Class methods (function inside class_method)
(class_method
  (function_declaration
    (function_body_declaration
      name: (simple_identifier) @name))) @definition.method

; Class methods (task inside class_method)
(class_method
  (task_declaration
    (task_body_declaration
      name: (simple_identifier) @name))) @definition.method

; Class constructor
(class_method
  (class_constructor_declaration)) @definition.constructor

; Standalone function declarations (not inside class)
(source_file
  (function_declaration
    (function_body_declaration
      name: (simple_identifier) @name))) @definition.function

; Standalone task declarations (not inside class)
(source_file
  (task_declaration
    (task_body_declaration
      name: (simple_identifier) @name))) @definition.function

; Module-level function declarations
(module_declaration
  (function_declaration
    (function_body_declaration
      name: (simple_identifier) @name))) @definition.function

; Module-level task declarations
(module_declaration
  (task_declaration
    (task_body_declaration
      name: (simple_identifier) @name))) @definition.function

; Package import declarations
(package_import_declaration
  (package_import_item
    (simple_identifier) @name)) @reference.import

; Include directives
(include_compiler_directive
  (quoted_string
    (quoted_string_item) @name)) @reference.includes

; Class extends
(class_declaration
  (class_type
    (simple_identifier) @name)) @reference.extends

; Module instantiations
(module_instantiation
  instance_type: (simple_identifier) @name) @reference.call
