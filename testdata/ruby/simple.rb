require 'json'
require_relative './helper'

module Greetings
  class Greeter
    def greet(name)
      "Hello, #{name}!"
    end

    private

    def format_name(name)
      name.capitalize
    end
  end

  class FormalGreeter < Greeter
    def greet(name)
      "Good day, #{format_name(name)}."
    end
  end
end

def standalone_function(input)
  greeter = Greetings::FormalGreeter.new
  greeter.greet(input)
end
