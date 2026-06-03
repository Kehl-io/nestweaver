; Method definitions (both instance and top-level)
(method
  name: (identifier) @name) @definition.method

; Singleton methods (class-level def self.foo)
(singleton_method
  name: (identifier) @name) @definition.method

; Class declarations
(class
  name: (constant) @name) @definition.class

; Module declarations
(module
  name: (constant) @name) @definition.module

; Constant assignments (CONST = value)
(assignment
  left: (constant) @name) @definition.const

; Call expressions (receiver.method calls)
(call
  method: (identifier) @name) @reference.call

; Instance variable assignment: @name = value
(assignment
  left: (instance_variable) @name) @definition.property

; Superclass (inheritance)
(class
  superclass: (superclass
    (constant) @name)) @reference.extends

; Scoped superclass (e.g. Foo::Bar)
(class
  superclass: (superclass
    (scope_resolution
      name: (constant) @name))) @reference.extends
