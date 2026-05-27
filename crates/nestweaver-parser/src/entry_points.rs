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
        "lua" => detect_lua(name, file_path, kind, signature),
        "bash" => detect_bash(name, file_path, kind, signature),
        "scala" => detect_scala(name, file_path, kind, signature),
        "elixir" => detect_elixir(name, file_path, kind, signature),
        "zig" => detect_zig(name, file_path, kind, signature),
        "objc" => detect_objc(name, file_path, kind, signature),
        "groovy" => detect_groovy(name, file_path, kind, signature),
        "powershell" => detect_powershell(name, file_path, kind, signature),
        "julia" => detect_julia(name, file_path, kind, signature),
        "sql" => None,
        "hcl" => None,
        "fortran" => detect_fortran(name, file_path, kind, signature),
        "pascal" => detect_pascal(name, file_path, kind, signature),
        _ => None,
    }
}

fn detect_js_ts(
    name: &str,
    file_path: &str,
    kind: &str,
    signature: Option<&str>,
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

    // ── Test files ──
    //
    // Any exported symbol in a test file is a test entry point. Covers:
    //   - __tests__/ directories (Jest convention)
    //   - *.test.{ts,js,tsx,jsx} and *.spec.{ts,js,tsx,jsx} files
    if matches!(kind, "function" | "class" | "constant") {
        let is_test_file = file_path.contains("/__tests__/")
            || file_name.contains(".test.")
            || file_name.contains(".spec.");
        if is_test_file && !name.starts_with('_') {
            return Some(EntryPointKind::TestEntry);
        }
    }

    // ── Config files ──
    //
    // Config files are entry points for build tools, linters, etc.
    // Covers: *.config.{ts,js,mjs,cjs}, vite.config.ts, next.config.js, etc.
    if matches!(kind, "function" | "class" | "constant") && file_name.contains(".config.") {
        return Some(EntryPointKind::Main);
    }

    // ── React / Next.js / TanStack Router / Remix page & layout entry points ──
    //
    // Files under /pages/, /app/, or /routes/ are framework entry points.
    // Exported functions, classes, and constants (e.g. `export const Route = ...`)
    // from these directories are treated as entry points.
    let is_page_path = file_path.contains("/pages/")
        || file_path.contains("/app/")
        || file_path.contains("/routes/");

    if is_page_path
        && matches!(kind, "function" | "class" | "constant")
        && !name.starts_with('_')
        && !name.starts_with("validate")
        && !name.starts_with("parse")
        && !name.starts_with("format")
    {
        return Some(EntryPointKind::HttpHandler);
    }

    // HTTP handlers in explicit handler/controller directories
    if matches!(kind, "function" | "class" | "constant")
        && (file_path.contains("/handlers/") || file_path.contains("/controllers/"))
        && !name.starts_with('_')
        && !name.starts_with("validate")
        && !name.starts_with("parse")
        && !name.starts_with("format")
    {
        return Some(EntryPointKind::HttpHandler);
    }

    // ── API route / tRPC / server handler entry points ──
    //
    // Exported routers (tRPC, Express), API handlers, and middleware are
    // framework entry points regardless of directory structure.
    if let Some(sig) = signature {
        // tRPC router definitions: `const fooRouter = router({`
        if name.ends_with("Router") && sig.contains("router(") {
            return Some(EntryPointKind::HttpHandler);
        }
        // Express / Koa / Hono route registration
        if sig.contains("app.get(")
            || sig.contains("app.post(")
            || sig.contains("app.put(")
            || sig.contains("app.delete(")
            || sig.contains("app.patch(")
            || sig.contains("app.use(")
            || sig.contains("router.get(")
            || sig.contains("router.post(")
            || sig.contains("router.put(")
            || sig.contains("router.delete(")
        {
            return Some(EntryPointKind::HttpHandler);
        }
        // Next.js API route handlers
        if (name == "GET" || name == "POST" || name == "PUT" || name == "DELETE" || name == "PATCH")
            && is_page_path
        {
            return Some(EntryPointKind::HttpHandler);
        }
        // Next.js special exports: getServerSideProps, getStaticProps, loader, action
        if name == "getServerSideProps"
            || name == "getStaticProps"
            || name == "getStaticPaths"
            || name == "loader"
            || name == "action"
        {
            return Some(EntryPointKind::HttpHandler);
        }
    }

    // ── React component entry points ──
    //
    // Uppercase-starting functions/classes in component directories are React
    // components and serve as UI entry points. This catches default and named
    // component exports.
    if matches!(kind, "function" | "class")
        && name.starts_with(|c: char| c.is_uppercase())
        && file_path.contains("/components/")
    {
        return Some(EntryPointKind::EventListener);
    }

    // ── Barrel / index files ──
    //
    // Files named index.{ts,js,tsx,jsx} serve as barrel re-export files or
    // package entry points. Exported symbols from these files are entry points
    // because they form the public API surface of their directory.
    let is_index_file = file_name == "index.ts"
        || file_name == "index.js"
        || file_name == "index.tsx"
        || file_name == "index.jsx"
        || file_name == "index.mjs"
        || file_name == "index.cjs";
    if is_index_file && matches!(kind, "function" | "class" | "constant") && !name.starts_with('_')
    {
        return Some(EntryPointKind::Main);
    }

    // ── Main / entry files ──
    //
    // Common entry file names at the root or src/ level: main.ts, app.ts, etc.
    let is_entry_file = file_name == "main.ts"
        || file_name == "main.js"
        || file_name == "main.tsx"
        || file_name == "main.jsx"
        || file_name == "app.ts"
        || file_name == "app.js"
        || file_name == "app.tsx"
        || file_name == "app.jsx"
        || file_name == "server.ts"
        || file_name == "server.js";
    if is_entry_file && matches!(kind, "function" | "class" | "constant") && !name.starts_with('_')
    {
        return Some(EntryPointKind::Main);
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

fn detect_julia(
    name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if name.starts_with("test_") {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_fortran(
    name: &str,
    _file_path: &str,
    kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    // program blocks are entry points
    if name.eq_ignore_ascii_case("main") && kind == "module" {
        return Some(EntryPointKind::Main);
    }
    None
}

fn detect_pascal(
    name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    // program declarations are entry points
    if name.eq_ignore_ascii_case("main") {
        return Some(EntryPointKind::Main);
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

fn detect_lua(
    name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    None
}

fn detect_bash(
    name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    None
}

fn detect_scala(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if let Some(sig) = signature
        && sig.contains("@main")
    {
        return Some(EntryPointKind::Main);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test")
        && (file_name.ends_with("Test.scala") || file_name.ends_with("Spec.scala"))
    {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_elixir(
    name: &str,
    file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test") && file_name.ends_with("_test.exs") {
        return Some(EntryPointKind::TestEntry);
    }
    if file_path.contains("/controllers/") {
        return Some(EntryPointKind::HttpHandler);
    }
    None
}

fn detect_zig(
    name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if name.starts_with("test") || name.starts_with("test_") {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_objc(
    name: &str,
    file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if name == "applicationDidFinishLaunching"
        || name == "didFinishLaunchingWithOptions"
        || name == "application"
    {
        return Some(EntryPointKind::Main);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test") && file_name.contains("Test") {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_groovy(
    name: &str,
    file_path: &str,
    _kind: &str,
    signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "main" {
        return Some(EntryPointKind::Main);
    }
    if let Some(sig) = signature
        && sig.contains("static void main(")
    {
        return Some(EntryPointKind::Main);
    }
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if name.starts_with("test")
        && (file_name.ends_with("Test.groovy") || file_name.ends_with("Spec.groovy"))
    {
        return Some(EntryPointKind::TestEntry);
    }
    None
}

fn detect_powershell(
    name: &str,
    _file_path: &str,
    _kind: &str,
    _signature: Option<&str>,
) -> Option<EntryPointKind> {
    if name == "Main" || name == "main" {
        return Some(EntryPointKind::Main);
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
    fn detects_react_route_constant() {
        let result = detect_entry_point(
            "Route",
            "src/routes/index.tsx",
            "constant",
            Some("const Route = createFileRoute"),
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_nextjs_page_function() {
        let result = detect_entry_point(
            "HomePage",
            "src/pages/index.tsx",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_nextjs_app_constant() {
        let result = detect_entry_point(
            "Dashboard",
            "src/app/dashboard/page.tsx",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_trpc_router() {
        let result = detect_entry_point(
            "accountRouter",
            "src/trpc/routers/account.ts",
            "constant",
            Some("const accountRouter = router({"),
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
    }

    #[test]
    fn detects_react_component_in_components_dir() {
        let result = detect_entry_point(
            "UserProfile",
            "src/components/user/UserProfile.tsx",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::EventListener));
    }

    #[test]
    fn lowercase_helper_in_components_not_entry() {
        let result = detect_entry_point(
            "formatDate",
            "src/components/utils.ts",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn detects_get_server_side_props() {
        let result = detect_entry_point(
            "getServerSideProps",
            "src/pages/users.tsx",
            "function",
            Some("export async function getServerSideProps("),
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::HttpHandler));
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
        let result = detect_entry_point("foo", "src/foo.unknown", "function", None, "unknown");
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

    // ── New entry point pattern tests ──

    #[test]
    fn detects_test_file_in_tests_dir() {
        let result = detect_entry_point(
            "renderUserProfile",
            "src/__tests__/UserProfile.test.tsx",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_spec_file() {
        let result = detect_entry_point(
            "LoginForm",
            "src/components/LoginForm.spec.tsx",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_test_file_constant() {
        let result = detect_entry_point(
            "mockData",
            "src/__tests__/fixtures.ts",
            "constant",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::TestEntry));
    }

    #[test]
    fn detects_config_file() {
        let result = detect_entry_point(
            "defineConfig",
            "vite.config.ts",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_next_config() {
        let result = detect_entry_point(
            "nextConfig",
            "next.config.js",
            "constant",
            None,
            "javascript",
        );
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_barrel_index_file() {
        let result = detect_entry_point(
            "UserService",
            "src/services/index.ts",
            "class",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_barrel_index_constant() {
        let result = detect_entry_point("api", "src/lib/index.ts", "constant", None, "typescript");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_main_ts_entry_file() {
        let result = detect_entry_point("createApp", "src/main.ts", "function", None, "typescript");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_app_tsx_entry_file() {
        let result = detect_entry_point("App", "src/app.tsx", "function", None, "typescript");
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn detects_server_ts_entry_file() {
        let result = detect_entry_point(
            "startServer",
            "src/server.ts",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, Some(EntryPointKind::Main));
    }

    #[test]
    fn private_in_test_file_not_entry() {
        let result = detect_entry_point(
            "_helper",
            "src/__tests__/utils.ts",
            "function",
            None,
            "typescript",
        );
        assert_eq!(result, None);
    }
}
