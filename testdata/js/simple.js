const fs = require('fs');

import { EventEmitter } from 'events';

function greet(name) {
    return 'Hello, ' + name;
}

const add = (a, b) => a + b;

class Animal {
    constructor(name) {
        this.name = name;
    }

    speak() {
        return this.name + ' makes a noise.';
    }
}

class Dog extends Animal {
    speak() {
        return this.name + ' barks.';
    }
}

const dog = new Dog('Rex');
greet('world');
dog.speak();
