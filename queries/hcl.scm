; ── HCL symbol extraction ────────────────────────────────────────────
; Grammar: tree-sitter-hcl
; HCL blocks: identifier string_lit* { body }
; e.g. resource "aws_instance" "web" { ... }

; Block definitions — capture the block keyword and first string label
; The @_keyword anchor ensures we match the identifier (resource/variable/etc.)
; followed by the first string_lit which is the resource type or name.
(block
  (identifier) @_keyword
  (string_lit) @name) @definition.class

; Function calls (lookup, file, templatefile, etc.)
(function_call
  (identifier) @name) @reference.call
