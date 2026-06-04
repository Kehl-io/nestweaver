; Function definitions
(function_statement
  (function_name) @name) @definition.function

; Class definitions
(class_statement
  (simple_name) @name) @definition.class

; Enum definitions
(enum_statement
  (simple_name) @name) @definition.enum

; Class method definitions
(class_method_definition
  (simple_name) @name) @definition.method

; Command invocations (cmdlet calls like Get-Process, Import-Module)
(command
  command_name: (command_name_expr) @name) @reference.call

(command
  command_name: (command_name) @name) @reference.call
