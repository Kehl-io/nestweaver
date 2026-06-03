; ── Ruby type extraction ─────────────────────────────────────────────
; Ruby is dynamically typed — constructor calls are the main source.

; Constructor: x = Foo.new(...)
(assignment
  left: (identifier) @ctor.name
  right: (call
    receiver: (constant) @ctor.type
    method: (identifier) @_method
    (#eq? @_method "new")))
