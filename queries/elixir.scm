; Module definitions: defmodule ModName do ... end
(call
  target: (identifier) @_defmodule
  (arguments
    (alias) @name)
  (#eq? @_defmodule "defmodule")) @definition.module

; Public function definitions: def func_name(...) do ... end
(call
  target: (identifier) @_def
  (arguments
    (call
      target: (identifier) @name))
  (#eq? @_def "def")) @definition.function

; Private function definitions: defp func_name(...) do ... end
(call
  target: (identifier) @_defp
  (arguments
    (call
      target: (identifier) @name))
  (#eq? @_defp "defp")) @definition.function

; Macro definitions: defmacro macro_name(...) do ... end
(call
  target: (identifier) @_defmacro
  (arguments
    (call
      target: (identifier) @name))
  (#eq? @_defmacro "defmacro")) @definition.function

; Private macro definitions: defmacrop macro_name(...) do ... end
(call
  target: (identifier) @_defmacrop
  (arguments
    (call
      target: (identifier) @name))
  (#eq? @_defmacrop "defmacrop")) @definition.function

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
