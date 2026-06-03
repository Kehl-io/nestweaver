; ── Lua type extraction ──────────────────────────────────────────────
; Lua is dynamically typed — there is no type annotation syntax in the
; language specification (5.1 through 5.4).
;
; Checked the tree-sitter-lua grammar (v0.5.0) node types:
;   - function_declaration: fields = {body, name, parameters} — no type field
;   - function_definition:  fields = {body, parameters} — no type field
;   - function_call:        fields = {arguments, name} — no type field
;   - assignment_statement: fields = {operator}; children = [variable_list, expression_list]
;   - parameters:           fields = {name}; children = [vararg_expression]
;
; No node type carries type information. Lua has no:
;   - Type annotations on variables or parameters
;   - Typed function return declarations
;   - Constructor keywords (new) — construction uses table constructors or
;     method calls (MyClass:new()) which are indistinguishable from regular calls
;
; LuaLS/EmmyLua-style type annotations (---@param, ---@type) live inside
; comments and are not parsed into the AST by tree-sitter-lua.
;
; Result: no type extraction patterns are possible for Lua.
