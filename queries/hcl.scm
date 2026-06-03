; Block definitions (resource, data, variable, output, module, provider, etc.)
; HCL blocks have the form: identifier [string_lit...] { body }
; We capture the block node and extract identifiers from it
(block
  (identifier) @name) @definition.class

; Function calls (e.g., lookup, file, templatefile)
(function_call
  (identifier) @name) @reference.call
