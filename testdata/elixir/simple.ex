defmodule Greeter do
  @moduledoc "A simple greeter module"

  use GenServer
  import Enum
  alias Greeter.Formatter

  def greet(name) do
    "Hello, #{format_name(name)}!"
  end

  defp format_name(name) do
    String.capitalize(name)
  end

  defmacro greeting_macro(name) do
    quote do
      "Hi, #{unquote(name)}!"
    end
  end
end

defmodule Greeter.Formatter do
  def format(text) do
    String.trim(text)
  end
end
