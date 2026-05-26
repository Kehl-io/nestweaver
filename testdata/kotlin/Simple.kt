package com.example

import com.example.utils.Helper

interface Greeter {
    fun greet(name: String): String
}

class SimpleGreeter : Greeter {
    override fun greet(name: String): String {
        return "Hello, $name!"
    }

    private fun logGreeting(message: String) {
        println(message)
    }
}

object AppConfig {
    val defaultName = "World"
}

fun main() {
    val greeter = SimpleGreeter()
    val result = greeter.greet(AppConfig.defaultName)
    println(result)
    Helper.assist()
}
