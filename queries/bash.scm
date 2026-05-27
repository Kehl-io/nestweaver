; Function definitions
(function_definition
  name: (word) @name) @definition.function

; Command invocations (function calls)
(command
  name: (command_name
    (word) @name)) @reference.call

; Source/dot includes
(command
  name: (command_name
    (word) @name)
  argument: (word) @reference.import
  (#match? @name "^(source|\\.)$"))
