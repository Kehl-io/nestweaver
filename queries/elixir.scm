; Module definitions: defmodule ModName do ... end
(call
  target: (identifier) @_defmodule
  (arguments
    (alias) @name)
  (#eq? @_defmodule "defmodule")) @definition.module

; Function-like declarations accept parenthesized calls, zero-arity names,
; guarded heads, and keyword-bodied declarations (def two, do: ...).
(call
  target: (identifier) @_def
  (arguments
    [
      (call target: (identifier) @name)
      (identifier) @name
      (binary_operator
        left: [
          (call target: (identifier) @name)
          (identifier) @name
        ]
        operator: "when")
    ])
  (#match? @_def "^(def|defp|defmacro|defmacrop|defguard|defguardp|defdelegate)$")) @definition.function

; Function calls
(call
  target: (identifier) @name
  (#not-match? @name "^(def|defp|defmodule|defmacro|defmacrop|defstruct|defimpl|defprotocol|defguard|defguardp|defdelegate|defoverridable|defexception|import|alias|use|require|if|unless|case|cond|with|for|raise|reraise|try|receive|send|spawn|spawn_link|fn|quote|unquote|super)$")) @reference.call

(call
  target: (dot
    right: (identifier) @name)) @reference.call

; Module attributes (@moduledoc, @doc, custom attributes)
(unary_operator
  operator: "@"
  operand: (call
    target: (identifier) @name)) @definition.const

; Alias references (like use, import, alias directives)
(call
  target: (identifier) @_use
  (arguments
    (alias) @name)
  (#eq? @_use "use")) @reference.uses

(call
  target: (identifier) @_import
  (arguments
    (alias) @name)
  (#eq? @_import "import")) @reference.import

(call
  target: (identifier) @_alias
  (arguments
    (alias) @name)
  (#eq? @_alias "alias")) @reference.import
