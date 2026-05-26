import Foundation

protocol Greeter {
    func greet(name: String) -> String
}

class SimpleGreeter: Greeter {
    func greet(name: String) -> String {
        return "Hello, \(name)!"
    }

    private func formatName(_ name: String) -> String {
        return name.capitalized
    }
}

struct AppConfig {
    let defaultName: String = "World"
}

enum Priority {
    case low
    case medium
    case high
}

func main() {
    let greeter = SimpleGreeter()
    let config = AppConfig()
    let result = greeter.greet(name: config.defaultName)
    print(result)
}
