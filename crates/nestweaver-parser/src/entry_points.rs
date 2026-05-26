use nestweaver_schema::EntryPointKind;

/// Detect whether a symbol is an entry point based on its name, file path,
/// kind (function/method/class), signature, and language.
///
/// Returns `Some(EntryPointKind)` if the symbol matches known entry point
/// patterns, `None` otherwise.
pub fn detect_entry_point(
    name: &str,
    file_path: &str,
    kind: &str,
    signature: Option<&str>,
    language: &str,
) -> Option<EntryPointKind> {
    match language {
        "javascript" | "typescript" => detect_js_ts(name, file_path, kind, signature),
        "python" => detect_python(name, file_path, kind, signature),
        "java" => detect_java(name, file_path, kind, signature),
        "go" => detect_go(name, file_path, kind, signature),
        "c" => detect_c(name, file_path, kind, signature),
        "csharp" => detect_csharp(name, file_path, kind, signature),
        "kotlin" => detect_kotlin(name, file_path, kind, signature),
        "php" => detect_php(name, file_path, kind, signature),
        "ruby" => detect_ruby(name, file_path, kind, signature),
        "dart" => detect_dart(name, file_path, kind, signature),
        "swift" => detect_swift(name, file_path, kind, signature),
        "cobol" => detect_cobol(name, file_path, kind, signature),
        "rust" => detect_rust(name, file_path, kind, signature),
        "cpp" => detect_cpp(name, file_path, kind, signature),
        _ => None,
    }
}

fn detect_js_ts(
    name: &str,
    file_path: &str,
    kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);

    // Lambda handler: file named lambda.{js,ts} or handler.{js,ts} AND named handler
    if (file_name == "lambda.js"
        || file_name == "lambda.ts"
        || file_name == "handler.js"
        || file_name == "handler.ts")
        && name == "handler"
    {
        return Some(EntryPointKind::LambdaHandler);
    }

    // Test framework functions (exact names only to avoid "iterator", "description", etc.)
    if name == "it" || name == "describe" || name == "test" || name.starts_with("test_") {
        return Some(EntryPointKind::TestEntry);
    }

    // HTTP handlers: only exported/top-level functions in route-like paths
    if kind == "function"
        && (file_path.contains("/routes/")
            || file_path.contains("/handlers/")
            || file_path.contains("/controllers/"))
    {
        // Skip obvious helpers: names starting with underscore or common utility prefixes
        if !name.starts_with('_')
            && !name.starts_with("validate")
            && !name.starts_with("parse")
            && !name.starts_with("format")
        {
            return Some(EntryPointKind::HttpHandler);
        }
    }

    // HTTP handler by name
    if name == "handler" || name == "middleware" {
        return Some(EntryPointKind::HttpHandler);
    }

    // Main entry points
    if name == "main" || name == "init" || name == "bootstrap" || name == "start" || name == "setup"
    {
        return Some(EntryPointKind::Main);
    }

    None
}

fn detect_python(
    name: &str,
    file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    // CLI command in Django management commands
    if file_path.contains("/management/commands/") {
        return Some(EntryPointKind::CliCommand);
    }

    // Lambda handler
    if name == "handler" || name == "lambda_handler" {
        return Some(EntryPointKind::LambdaHandler);
    }

    // Test entries
    if name.starts_with("test_") {
        return Some(EntryPointKind::TestEntry);
    }

    // HTTP handler by file name
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name == "views.py"
        || file_name == "routes.py"
        || file_name == "endpoints.py"
        || file_name == "handlers.py"
    {
        return Some(EntryPointKind::HttpHandler);
    }

    // Main entry point
    if name == "main" {
        return Some(EntryPointKind::Main);
    }

    None
}

fn detect_java(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    // Main method
    if let Some(sig) = signature
        && sig.contains("public static void main(String")
    {
        return Some(EntryPointKind::Main);
    }

    // HTTP handler methods
    if name == "doGet" || name == "doPost" || name == "doPut" || name == "doDelete" {
        return Some(EntryPointKind::HttpHandler);
    }

    // HTTP handler by file name pattern
    if file_path.ends_with("Controller.java") || file_path.ends_with("Handler.java") {
        return Some(EntryPointKind::HttpHandler);
    }

    // Test methods
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test")
        && (file_name.ends_with("Test.java") || file_name.ends_with("Tests.java"))
    {
        return Some(EntryPointKind::TestEntry);
    }

    None
}

fn detect_go(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    // Test/Benchmark functions
    if name.starts_with("Test") || name.starts_with("Benchmark") {
        return Some(EntryPointKind::TestEntry);
    }

    // HTTP handler by signature
    if let Some(sig) = signature
        && (sig.contains("http.ResponseWriter") || sig.contains("*http.Request"))
    {
        return Some(EntryPointKind::HttpHandler);
    }

    // HTTP handler by name
    if name == "Handler" || name == "Handle" || name.ends_with("Handler") {
        return Some(EntryPointKind::HttpHandler);
    }

    // Main function
    if name == "main" && (file_path.contains("cmd/") || file_path.ends_with("main.go")) {
        return Some(EntryPointKind::Main);
    }

    // Init function
    if name == "init" {
        return Some(EntryPointKind::Main);
    }

    None
}

fn detect_c(
    name: &str,
    _file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if let Some(sig) = signature
        && (sig.contains("int main(") || sig.contains("void main("))
    {
        return Some(EntryPointKind::Main);
    }
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if name == "setup" {
        return Some(EntryPointKind::Main);
    }
    if name.starts_with("test_") {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_csharp(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if let Some(sig) = signature
        && (sig.contains("static void Main(") || sig.contains("static async Task Main("))
    {
        return Some(EntryPointKind::Main);
    }
    if file_path.ends_with("Controller.cs") {
        return Some(EntryPointKind::HttpHandler);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if (name.starts_with("Test") || name.starts_with("test"))
        && (file_name.ends_with("Test.cs") || file_name.ends_with("Tests.cs"))
    {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_kotlin(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test") && file_name.ends_with("Test.kt") {
        return Some(EntryPointKind::TestEntry);
    }
    if let Some(sig) = signature
        && (sig.contains("@Controller") || sig.contains("@RestController"))
    {
        return Some(EntryPointKind::HttpHandler);
    }
    None
}

fn detect_php(
    name: &str,
    file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name.ends_with("Controller.php") {
        return Some(EntryPointKind::HttpHandler);
    }
    if file_path.contains("/routes/") {
        return Some(EntryPointKind::HttpHandler);
    }
    if name.starts_with("test") && file_name.ends_with("Test.php") {
        return Some(EntryPointKind::TestEntry);
    }
    if name == "handle" {
        return Some(EntryPointKind::CliCommand);
    }
    None
}

fn detect_ruby(
    name: &str,
    file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name.ends_with("_controller.rb") {
        return Some(EntryPointKind::HttpHandler);
    }
    if name == "describe" || name == "it" || name == "context" {
        return Some(EntryPointKind::TestEntry);
    }
    if name.starts_with("test_") {
        return Some(EntryPointKind::TestEntry);
    }
    if file_name.ends_with(".rake") {
        return Some(EntryPointKind::CliCommand);
    }
    if name == "perform" || name == "call" {
        return Some(EntryPointKind::EventListener);
    }
    None
}

fn detect_dart(
    name: &str,
    file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if name == "test" || name == "testWidgets" || name == "group" {
        return Some(EntryPointKind::TestEntry);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name.ends_with("_test.dart") {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_swift(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if let Some(sig) = signature
        && (sig.contains("@main") || sig.contains("didFinishLaunchingWithOptions"))
    {
        return Some(EntryPointKind::Main);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test") && file_name.ends_with("Tests.swift") {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_cobol(
    _name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    None // COBOL entry points handled in the cobol parser directly
}

fn detect_rust(
    name: &str,
    _file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if let Some(sig) = signature {
        if sig.contains("#[test]") || sig.contains("#[tokio::test]") {
            return Some(EntryPointKind::TestEntry);
        }
        if sig.contains("#[tokio::main]") || sig.contains("#[actix_web::main]") {
            return Some(EntryPointKind::Main);
        }
    }
    None
}

fn detect_cpp(
    name: &str,
    _file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if let Some(sig) = signature
        && sig.contains("int main(")
    {
        return Some(EntryPointKind::Main);
    }
    if name == "TEST" || name == "TEST_F" || name == "TEST_P" {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_js_main() {
        let result = detect_entry_point("main", "src/index.js", "function", None, "javascript");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_js_bootstrap() {
        let result = detect_entry_point("bootstrap", "src/app.ts", "function", None, "typescript");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_js_route_handler() {
        let result = detect_entry_point(
            "getUser",
            "src/routes/users.ts",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_js_lambda_handler() {
        let result =
            detect_entry_point("handler", "src/handler.ts", "function", None, "typescript");
        assert_eq!(result, Some(EntryPointKind::LambdaHandler));
    }

    #[test]
    fn detects_js_test_exact() {
        let result = detect_entry_point("test", "tests/auth.js", "function", None, "javascript");
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_js_test_prefixed() {
        let result = detect_entry_point(
            "test_login",
            "tests/auth.js",
            "function",
            None,
            "javascript",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn js_iterator_is_not_test() {
        let result = detect_entry_point("iterator", "src/utils.js", "function", None, "javascript");
        assert_eq!(result, None);
    }

    #[test]
    fn js_describe_route_is_not_test() {
        let result = detect_entry_point(
            "describeRoute",
            "src/api.js",
            "function",
            None,
            "javascript",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn detects_python_test() {
        let result = detect_entry_point(
            "test_login",
            "tests/test_auth.py",
            "function",
            None,
            "python",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_python_main() {
        let result = detect_entry_point("main", "src/app.py", "function", None, "python");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_python_views() {
        let result = detect_entry_point("user_list", "myapp/views.py", "function", None, "python");
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_python_lambda_handler() {
        let result = detect_entry_point(
            "lambda_handler",
            "src/handler.py",
            "function",
            None,
            "python",
        );
        assert_eq!(result, Some(EntryPointKind::LambdaHandler));
    }

    #[test]
    fn detects_python_cli_command() {
        let result = detect_entry_point(
            "handle",
            "myapp/management/commands/migrate.py",
            "method",
            None,
            "python",
        );
        assert_eq!(result, Some(EntryPointKind::CliCommand));
    }

    #[test]
    fn detects_go_http_handler() {
        let result = detect_entry_point(
            "HandleUsers",
            "pkg/api/users.go",
            "function",
            Some("func HandleUsers(w http.ResponseWriter, r *http.Request)"),
            "go",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_go_main() {
        let result = detect_entry_point("main", "cmd/server/main.go", "function", None, "go");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_go_test() {
        let result = detect_entry_point(
            "TestCreateUser",
            "pkg/api/users_test.go",
            "function",
            None,
            "go",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_go_benchmark() {
        let result = detect_entry_point(
            "BenchmarkSort",
            "pkg/sort/sort_test.go",
            "function",
            None,
            "go",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_go_init() {
        let result = detect_entry_point("init", "pkg/config/config.go", "function", None, "go");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_java_main() {
        let result = detect_entry_point(
            "main",
            "src/Main.java",
            "method",
            Some("public static void main(String[] args)"),
            "java",
        );
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_java_http_handler() {
        let result = detect_entry_point("doGet", "src/UserServlet.java", "method", None, "java");
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_java_controller() {
        let result = detect_entry_point(
            "getUsers",
            "src/UserController.java",
            "method",
            None,
            "java",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_java_test() {
        let result = detect_entry_point(
            "testGetUser",
            "src/UserServiceTest.java",
            "method",
            None,
            "java",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn js_validate_in_routes_is_not_handler() {
        let result = detect_entry_point(
            "validateInput",
            "src/routes/users.ts",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn non_entry_point_returns_none() {
        let result = detect_entry_point(
            "calculateTotal",
            "src/utils.js",
            "function",
            None,
            "javascript",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn detects_cpp_main() {
        let result = detect_entry_point("main", "src/main.cpp", "function", None, "cpp");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_cpp_test_macro() {
        let result = detect_entry_point("TEST_F", "tests/suite.cpp", "function", None, "cpp");
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_rust_main() {
        let result = detect_entry_point("main", "src/main.rs", "function", None, "rust");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_rust_test() {
        let result = detect_entry_point(
            "it_parses",
            "src/lib.rs",
            "function",
            Some("#[test] fn it_parses()"),
            "rust",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_rust_tokio_main() {
        let result = detect_entry_point(
            "main",
            "src/main.rs",
            "function",
            Some("#[tokio::main] async fn main()"),
            "rust",
        );
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn unsupported_language_returns_none() {
        let result = detect_entry_point("foo", "src/foo.zig", "function", None, "zig");
        assert_eq!(result, None);
    }

    #[test]
    fn detects_kotlin_main() {
        let result = detect_entry_point("main", "src/Main.kt", "function", None, "kotlin");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_kotlin_test() {
        let result = detect_entry_point(
            "testGreeting",
            "src/GreeterTest.kt",
            "function",
            None,
            "kotlin",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn kotlin_non_entry_returns_none() {
        let result = detect_entry_point("greet", "src/Greeter.kt", "function", None, "kotlin");
        assert_eq!(result, None);
    }
}
