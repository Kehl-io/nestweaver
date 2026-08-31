; Function definitions
(function_definition
  (signature
    (call_expression
      (identifier) @name))) @definition.function

; ...with a return-type annotation: `function greet(x)::String`. The annotation
; wraps the call in a `typed_expression`, so the pattern above -- which requires
; `call_expression` to be a DIRECT child of `signature` -- does not match, and
; `greet` at simple.jl:16 was never extracted. Found while fixing nw-364(1):
; the phantom `greet` minted from the CALL SITE at :35 was standing in for the
; real definition, so the gap was invisible in the symbol table.
(function_definition
  (signature
    (typed_expression
      . (call_expression
          . (identifier) @name)))) @definition.function

(function_definition
  (signature
    (identifier) @name)) @definition.function

; Short-form function definitions: f(x) = x + 1
;
; nw-364(1): the anchors are load-bearing. tree-sitter-julia's `assignment`
; node has NO lhs/rhs field names, so an unanchored `(call_expression ...)`
; matches ANY named child -- including the right-hand side. `greeting =
; greet(animal.name)` therefore minted `greet` as a DEFINITION, a bodiless
; one-line symbol that the degenerate-span fallback can then pick as the
; enclosing scope for a reference it does not contain. The leading `.` pins
; `call_expression` to the assignment's FIRST named child, which is the LHS.
(assignment
  . (call_expression
      . (identifier) @name)) @definition.function

; Struct definitions
(struct_definition
  (type_head
    (identifier) @name)) @definition.class

; ...with a supertype: `struct Animal <: LivingThing`. The `<:` clause makes
; `type_head` hold a `binary_expression` rather than a bare identifier, so
; `Animal` at simple.jl:7 was never extracted. Same masking as above: the
; phantom `Animal` minted from `Animal("Dog", "Woof")` at :34 stood in for it.
; The anchor takes the SUBTYPE, not the supertype.
(struct_definition
  (type_head
    (binary_expression
      . (identifier) @name))) @definition.class

; ...and the same shape for `abstract type Foo <: Bar end`.
(abstract_definition
  (type_head
    (binary_expression
      . (identifier) @name))) @definition.interface

; Module definitions
(module_definition
  name: (identifier) @name) @definition.module

; Macro definitions
(macro_definition
  (signature
    (call_expression
      (identifier) @name))) @definition.function

; Abstract type definitions
(abstract_definition
  (type_head
    (identifier) @name)) @definition.interface

; Import statements
(import_statement
  (import_path
    (identifier) @name)) @reference.import

(import_statement
  (identifier) @name) @reference.import

; Function calls
(call_expression
  (identifier) @name) @reference.call
