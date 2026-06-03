; ── Elixir type extraction ───────────────────────────────────────────

; Struct construction: user = %User{name: "test"}
; In the AST this is:
;   binary_operator (=)
;     left: identifier ("user")
;     right: map
;       struct > alias ("User")
(binary_operator
  left: (identifier) @ctor.name
  right: (map
    (struct
      (alias) @ctor.type)))
