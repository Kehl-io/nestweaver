; ── Pascal type extraction ───────────────────────────────────────────
; Pascal uses `var x: Type;` syntax. The tree-sitter-pascal grammar
; uses camelCase node names (declVar, declField, declProc, declArg).
; These nodes have named fields: name (identifier), type (type/typeref).
;
; declVar:   fields = {name: identifier, type: type}
; declField: fields = {name: identifier, type: type}
; declProc:  fields = {name: identifier, type: type/typeref, args: declArgs}
; declArg:   fields = {name: identifier, type: type}

; Variable declaration: var x: Integer;
; declVar.type accepts node type `type`, not `typeref`
(declVar
  name: (identifier) @var.name
  type: (_) @var.type)

; Field declaration in a class/record
(declField
  name: (identifier) @var.name
  type: (_) @var.type)

; Function return type: function Foo(): Integer;
; declProc.type accepts both `type` and `typeref`
(declProc
  name: (identifier) @return.name
  type: (_) @return.type)

; Procedure/function parameter: procedure Bar(x: Integer);
(declArg
  name: (identifier) @param.name
  type: (_) @param.type)
