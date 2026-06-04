; ── SQL symbol extraction ────────────────────────────────────────────
; Grammar: tree-sitter-sequel

; CREATE TABLE
(create_table
  (object_reference
     name: (identifier) @name)
) @definition.class

; CREATE VIEW
(create_view
  (object_reference
     name: (identifier) @name)
) @definition.class

; CREATE FUNCTION
(create_function
  (object_reference
     name: (identifier) @name)
) @definition.function

; CREATE TRIGGER
(create_trigger
  (object_reference
     name: (identifier) @name)
) @definition.function

; CREATE TYPE
(create_type
  (object_reference
     name: (identifier) @name)
) @definition.type

; Function invocations
(invocation
  (object_reference
     name: (identifier) @name)
) @reference.call

; Table references in FROM
(from
  (object_reference
     name: (identifier) @name)
) @reference.call
