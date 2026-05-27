local json = require("json")

-- A simple greeter table (acts as a class)
local Greeter = {}
Greeter.__index = Greeter

function Greeter:new(name)
    local instance = setmetatable({}, Greeter)
    instance.name = name
    return instance
end

function Greeter:greet()
    return "Hello, " .. self.name .. "!"
end

-- A global function
function format_name(name)
    return string.upper(name)
end

-- A local helper function
local function helper(input)
    return input:lower()
end

-- Usage
local g = Greeter:new("World")
print(g:greet())
print(format_name("test"))
