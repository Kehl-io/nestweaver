import 'package:flutter/material.dart';
import 'helper.dart';

abstract class Greeter {
  String greet(String name);
}

mixin Loggable {
  void log(String message) {
    print(message);
  }
}

class SimpleGreeter extends Greeter with Loggable {
  @override
  String greet(String name) {
    log('Greeting $name');
    return 'Hello, $name!';
  }

  String _formatName(String name) {
    return name.toUpperCase();
  }
}

enum Priority { low, medium, high }

void main() {
  final greeter = SimpleGreeter();
  final result = greeter.greet('World');
  print(result);
  assist();
}
