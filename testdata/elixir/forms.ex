defmodule Forms do
  def zero do
    :ok
  end

  def keyword, do: :ok
  def guarded(value) when value > 1 do
    value
  end

  defp private_guarded(value) when is_integer(value), do: value
  defmacro macro_zero do
    quote do: :ok
  end

  defmacro guarded_macro(value) when is_integer(value) do
    quote do: unquote(value)
  end

  defmacrop private_macro(value), do: value
  defguard positive(value) when value > 0
  defguardp nonzero(value) when value != 0
  defdelegate delegated(value), to: Target
  def multi(value) when value > 1, do: value
  def multi(0), do: 0
end
