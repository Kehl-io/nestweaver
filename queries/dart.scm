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

; Method DEFINITIONS (inside a class body).
;
; Anchored on `method_declaration`, not on `method_signature`. The signature is
; the method WITHOUT its body, so anchoring there recorded
; `end_line == start_line` for every Dart method — `greet` in
; testdata/dart/simple.dart read as 16-16 when it spans 16-19. The top-level
; `function_declaration` rule above was already anchored on the declaration and
; was always correct, which is what made the inconsistency invisible: `main` had
; a real span and every method in the same file did not.
(method_declaration
  signature: (method_signature
    (function_signature
      name: (identifier) @name))) @definition.method

; ABSTRACT method declarations — no body, so genuinely one line. In an abstract
; class or an interface these are a `class_member > declaration` wrapping a bare
; `function_signature`, which no `method_signature` rule could ever match, so
; `Greeter.greet` in testdata/dart/simple.dart was extracted as NOTHING. The
; same split as C++: definitions carry a body span, declarations do not.
(declaration
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
