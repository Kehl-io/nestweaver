; @interface ClassName — class interface declaration
(class_interface
  (identifier) @name) @definition.interface

; @implementation ClassName — class implementation
(class_implementation
  (identifier) @name) @definition.class

; @protocol ProtocolName — protocol declaration
(protocol_declaration
  (identifier) @name) @definition.interface

; Method definitions (- or + methods with body)
(method_definition
  (identifier) @name) @definition.method

; Method declarations (in @interface, no body)
(method_declaration
  (identifier) @name) @definition.method

; C-style function definitions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

; #import "header.h" or #import <header.h>
(preproc_include
  path: (string_literal) @name) @reference.import

(preproc_include
  path: (system_lib_string) @name) @reference.import

; Message sends: [receiver method]
(message_expression
  method: (identifier) @name) @reference.call

; C-style function calls
(call_expression
  function: (identifier) @name) @reference.call

