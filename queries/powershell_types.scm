; ── PowerShell type extraction ───────────────────────────────────────
; PowerShell uses [Type] annotations. The tree-sitter-powershell grammar
; represents these as positional children (not named fields).
;
; class_property_definition: children = [class_attribute*, type_literal?, variable]
; class_method_definition:   children = [class_attribute*, type_literal?, simple_name, class_method_parameter_list?, script_block]
; class_method_parameter:    children = [type_literal?, variable]
;
; Since these nodes use positional children (no named fields like type:/name:),
; we match them structurally via parent-child relationships.

; Class property with type: [string]$Name
(class_property_definition
  (type_literal) @var.type
  (variable) @var.name)

; Class method return type: [int] MyMethod() { ... }
(class_method_definition
  (type_literal) @return.type
  (simple_name) @return.name)

; Class method parameter: [string]$param
(class_method_parameter
  (type_literal) @param.type
  (variable) @param.name)
