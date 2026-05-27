import groovy.transform.CompileStatic
import java.util.logging.Logger

interface Greeter {
    String greet(String name)
}

trait Loggable {
    void log(String message) {
        println(message)
    }
}

class SimpleGreeter implements Greeter {
    String prefix

    SimpleGreeter(String prefix) {
        this.prefix = prefix
    }

    String greet(String name) {
        String formatted = formatName(name)
        return "${prefix} ${formatted}!"
    }

    private String formatName(String name) {
        return name.capitalize()
    }
}

class FormalGreeter extends SimpleGreeter {
    FormalGreeter() {
        super("Dear")
    }
}

enum Priority {
    LOW, MEDIUM, HIGH
}

def main(String[] args) {
    def greeter = new SimpleGreeter("Hello")
    println(greeter.greet("world"))
}
