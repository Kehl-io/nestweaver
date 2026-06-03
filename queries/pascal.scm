; ── Pascal symbol extraction ──────────────────────────────────────────

; Unit name
(unit
  (moduleName
    (identifier) @name)) @definition.module

; Type declarations (classes)
; declType with declClass child -> class
(declType
  name: (identifier) @name
  type: (declClass)) @definition.class

; Type declarations (interfaces) — not tested in fixture but included
; for completeness

; Procedure declarations in interface section
(interface
  (declProc
    (kProcedure)
    name: (identifier) @name)) @definition.function

; Function declarations in interface section
(interface
  (declProc
    (kFunction)
    name: (identifier) @name)) @definition.function

; Procedure/function implementations (defProc)
; Constructor implementations
(defProc
  header: (declProc
    (kConstructor)
    name: (genericDot
      rhs: (identifier) @name))) @definition.method

; Method implementations (procedure with dot name)
(defProc
  header: (declProc
    (kProcedure)
    name: (genericDot
      rhs: (identifier) @name))) @definition.method

(defProc
  header: (declProc
    (kFunction)
    name: (genericDot
      rhs: (identifier) @name))) @definition.method

; Standalone procedure implementations
(defProc
  header: (declProc
    (kProcedure)
    name: (identifier) @name)) @definition.function

; Standalone function implementations
(defProc
  header: (declProc
    (kFunction)
    name: (identifier) @name)) @definition.function

; Uses clause imports
(declUses
  (moduleName
    (identifier) @name)) @reference.import

; Class inheritance (parent type reference)
(declClass
  parent: (typeref
    (identifier) @name)) @reference.extends

; Function/procedure calls in implementation
(exprCall
  entity: (identifier) @name) @reference.call

; Inherited calls
(exprCall
  entity: (inherited
    (identifier) @name)) @reference.call
