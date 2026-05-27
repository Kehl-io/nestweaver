module Greetings

export greet, Animal

abstract type LivingThing end

struct Animal <: LivingThing
    name::String
    sound::String
end

mutable struct Counter
    value::Int
end

function greet(name::String)::String
    return "Hello, $name!"
end

function process(items::Vector{Int})
    for item in items
        println(item)
    end
end

macro log_call(expr)
    quote
        println("Calling: ", $(string(expr)))
        $(esc(expr))
    end
end

function main()
    animal = Animal("Dog", "Woof")
    greeting = greet(animal.name)
    println(greeting)
    @log_call process([1, 2, 3])
end

end # module
