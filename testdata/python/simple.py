from .utils import helper
import os

def standalone_function(name):
    return f"Hello, {name}!"

def decorator_factory(prefix):
    def decorator(func):
        def wrapper(*args, **kwargs):
            return prefix + func(*args, **kwargs)
        return wrapper
    return decorator

@decorator_factory(">>> ")
def decorated_greet(name):
    return f"Hello, {name}!"

class Animal:
    def __init__(self, name):
        self.name = name

    def speak(self):
        return f"{self.name} makes a noise."

class Dog(Animal):
    def speak(self):
        return f"{self.name} barks."

def main():
    dog = Dog("Rex")
    result = dog.speak()
    standalone_function("world")
    os.getcwd()
    return result

if __name__ == "__main__":
    main()
