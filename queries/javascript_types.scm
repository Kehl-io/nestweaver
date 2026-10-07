; JavaScript constructor bindings. TypeScript annotation nodes are absent
; from this grammar, so this query must compile independently.
(variable_declarator
  name: (identifier) @ctor.name
  value: (new_expression
    constructor: (identifier) @ctor.type))

(field_definition
  property: (property_identifier) @ctor.name
  value: (new_expression
    constructor: (identifier) @ctor.type))
