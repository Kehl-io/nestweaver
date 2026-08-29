; Function declarations
(function_declaration
  name: (identifier) @name) @definition.function

; Named function expressions assigned to variables
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function

; Export-wrapped arrow functions
(export_statement
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: (arrow_function)))) @definition.function

; Class declarations
[
  (class_declaration
    name: (identifier) @name)
  (class
    name: (identifier) @name)
] @definition.class

; Method definitions
(method_definition
  name: (property_identifier) @name) @definition.method

; Class properties (field definitions)
(field_definition
  property: (property_identifier) @name) @definition.property

; Const declarations (non-function values), MODULE SCOPE ONLY.
;
; nw-150: this previously matched at any nesting depth, so a block-local
; `const where = { class_id: ... }` inside an else-branch became a
; first-class graph symbol -- and, once call resolution bound `.where(..)`
; method calls to it, the single most-depended-on symbol in a 193k-symbol
; graph. Anchoring to `program`/`export_statement` keeps exported and
; module-level constants (which are genuine API surface) while excluding
; function- and block-locals, which are not addressable from outside.
(program
  (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      value: (_) @_val)
    (#not-match? @_val "^(\\(|function|class)")) @definition.const)

(export_statement
  (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      value: (_) @_val)
    (#not-match? @_val "^(\\(|function|class)")) @definition.const)

; Test-runner blocks (Jest/Vitest/Mocha): test('name', fn), it('name', fn),
; describe('name', fn). Captured as a definition so the calls inside the
; callback attach to this symbol (named after the test title).
(call_expression
  function: (identifier) @_runner
  arguments: (arguments
    (string) @name
    [(arrow_function) (function_expression)])
  (#match? @_runner "^(test|it|describe)$")) @definition.function

; Call expressions (function calls)
(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (member_expression
    property: (property_identifier) @name)) @reference.call

; Import statements (ES modules)
(import_statement
  source: (string) @name) @reference.import

; Re-export source: `export * from './x'` / `export { A } from './x'`.
;
; nw-323 (defect C): `export_statement` was captured only inside
; `@definition.*` patterns for export-wrapped declarations, so the re-export
; SOURCE was never captured as an import. A barrel file therefore had zero
; imports, `ImportGraph::imports_of` returned empty for it, and the re-export
; tier in resolve.rs had nothing to walk. Rust does not have this hole because
; `use_declaration` covers `pub use` and is captured whole.
(export_statement
  source: (string) @name) @reference.import

; Object construction: `new Foo(...)`.
;
; nw-323 (defect D): `new_expression` appeared in NEITHER typescript.scm nor
; javascript.scm -- only in the *_types.scm files, which feed the type
; environment and NOT the edge set (`resolve_references_with_context` takes that
; argument as `_type_envs`). `NotFoundError` and `NotificationService` are
; consumed almost exclusively via `new`, so they had ZERO inbound references
; before resolution even began -- which is why `impact --min-score 0 --depth 10`
; still returned 0 for them. The edges were ABSENT, not pruned.
(new_expression
  constructor: (identifier) @name) @reference.call

; require() calls
(call_expression
  function: (identifier)
  arguments: (arguments (string) @name)) @reference.import

; JSX opening element — component reference
(jsx_opening_element
  name: (identifier) @name) @reference.call

; JSX self-closing element — component reference
(jsx_self_closing_element
  name: (identifier) @name) @reference.call

; Instance property: this.prop = value
(assignment_expression
  left: (member_expression
    object: (this)
    property: (property_identifier) @name)) @definition.property

; ── Field access (ACCESSES edges) ───────────────────────────────────
; Property read: obj.field
(member_expression
  property: (property_identifier) @name) @reference.read_access
