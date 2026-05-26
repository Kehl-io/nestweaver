import { EventEmitter } from 'events';

interface Greeter {
    greet(name: string): string;
    farewell(name: string): string;
}

interface Logger {
    log(message: string): void;
}

class ConsoleGreeter implements Greeter, Logger {
    greet(name: string): string {
        return `Hello, ${name}!`;
    }

    farewell(name: string): string {
        return `Goodbye, ${name}!`;
    }

    log(message: string): void {
        console.log(message);
    }
}

class VerboseGreeter extends ConsoleGreeter {
    greet(name: string): string {
        this.log(`Greeting ${name}`);
        return super.greet(name);
    }
}

function createGreeter(): Greeter {
    return new ConsoleGreeter();
}

const greeter = createGreeter();
greeter.greet('world');
