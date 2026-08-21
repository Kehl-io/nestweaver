use crate::parse::RawSymbol;
use nestweaver_schema::{FrameworkHint, SymbolKind};

pub fn detect_frameworks(
    symbols: &[RawSymbol],
    file_path: &str,
    language: &str,
) -> Vec<(usize, FrameworkHint)> {
    // Returns (symbol_index, hint) pairs so caller can attach hints to the right symbols
    let mut hints = Vec::new();

    match language {
        "java" | "kotlin" => detect_spring(symbols, &mut hints),
        "csharp" => detect_aspnet(symbols, file_path, &mut hints),
        "python" => detect_django_flask(symbols, file_path, &mut hints),
        "javascript" | "typescript" => detect_express_nextjs(symbols, file_path, &mut hints),
        "ruby" => detect_rails(symbols, file_path, &mut hints),
        "php" => detect_laravel(symbols, file_path, &mut hints),
        "dart" => detect_flutter(symbols, &mut hints),
        "swift" => detect_swiftui(symbols, &mut hints),
        "go" => detect_go_frameworks(symbols, &mut hints),
        _ => {}
    }

    hints
}

fn detect_spring(symbols: &[RawSymbol], hints: &mut Vec<(usize, FrameworkHint)>) {
    for (i, sym) in symbols.iter().enumerate() {
        let sig = &sym.signature;
        if sig.contains("@Controller") || sig.contains("@RestController") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "spring".into(),
                    role: "controller".into(),
                },
            ));
        } else if sig.contains("@Service") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "spring".into(),
                    role: "service".into(),
                },
            ));
        } else if sig.contains("@Repository") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "spring".into(),
                    role: "repository".into(),
                },
            ));
        } else if sig.contains("@Component") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "spring".into(),
                    role: "component".into(),
                },
            ));
        }
    }
}

fn detect_aspnet(symbols: &[RawSymbol], file_path: &str, hints: &mut Vec<(usize, FrameworkHint)>) {
    for (i, sym) in symbols.iter().enumerate() {
        if sym.signature.contains("[ApiController]") || file_path.ends_with("Controller.cs") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "aspnet".into(),
                    role: "controller".into(),
                },
            ));
        }
    }
}

fn detect_django_flask(
    symbols: &[RawSymbol],
    file_path: &str,
    hints: &mut Vec<(usize, FrameworkHint)>,
) {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name == "views.py" || file_name == "viewsets.py" {
        for (i, sym) in symbols.iter().enumerate() {
            if matches!(
                sym.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Class
            ) {
                hints.push((
                    i,
                    FrameworkHint {
                        framework: "django".into(),
                        role: "view".into(),
                    },
                ));
            }
        }
    }
    for (i, sym) in symbols.iter().enumerate() {
        if sym.signature.contains("@app.route") || sym.signature.contains("@router.") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "flask".into(),
                    role: "handler".into(),
                },
            ));
        }
    }
}

fn detect_express_nextjs(
    symbols: &[RawSymbol],
    file_path: &str,
    hints: &mut Vec<(usize, FrameworkHint)>,
) {
    if file_path.contains("/pages/")
        || file_path.contains("/app/")
        || file_path.contains("/routes/")
    {
        for (i, sym) in symbols.iter().enumerate() {
            if matches!(
                sym.kind,
                SymbolKind::Function | SymbolKind::Class | SymbolKind::Constant
            ) {
                hints.push((
                    i,
                    FrameworkHint {
                        framework: "nextjs".into(),
                        role: "page".into(),
                    },
                ));
            }
        }
    }
    for (i, sym) in symbols.iter().enumerate() {
        if let Some(framework) = http_route_framework(&sym.signature) {
            hints.push((
                i,
                FrameworkHint {
                    framework: framework.into(),
                    role: "handler".into(),
                },
            ));
        }
    }
}

/// HTTP methods a Node route registration can name.
const HTTP_VERBS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "all",
];

/// Receivers that route registrations are called on, and the framework each
/// implies.
const ROUTE_RECEIVERS: &[(&str, &str)] = &[
    ("fastify", "fastify"),
    ("app", "express"),
    ("router", "express"),
    ("server", "express"),
    ("api", "express"),
];

/// The framework whose route registration `signature` looks like, if any.
///
/// nw-160: this matched exactly three literal substrings — `app.get(`,
/// `app.post(` and `router.`. Across 40 indexed repos that produced ONE HTTP
/// contract in the entire org graph, while coyote-measurement/server alone has
/// 412 Fastify route registrations. `fastify.get(` never matched, and neither
/// did plain Express `app.put(` / `app.delete(` / `app.patch(`. The knock-on
/// was that `contracts drift` reported declared_not_implemented 0 vacuously.
///
/// The broad `router.` prefix is deliberately KEPT alongside the verb-specific
/// patterns: narrowing it to verbs would drop `router.use(` and `router.param(`
/// registrations that already matched, trading one coverage gap for another.
fn http_route_framework(signature: &str) -> Option<&'static str> {
    if signature.contains("router.") {
        return Some("express");
    }
    for (receiver, framework) in ROUTE_RECEIVERS {
        // `fastify.route({ method: 'GET', ... })` and `app.route('/x')`.
        if signature.contains(&format!("{receiver}.route(")) {
            return Some(framework);
        }
        for verb in HTTP_VERBS {
            if signature.contains(&format!("{receiver}.{verb}(")) {
                return Some(framework);
            }
        }
    }
    None
}

fn detect_rails(symbols: &[RawSymbol], file_path: &str, hints: &mut Vec<(usize, FrameworkHint)>) {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name.ends_with("_controller.rb") {
        for (i, sym) in symbols.iter().enumerate() {
            if sym.kind == SymbolKind::Method {
                hints.push((
                    i,
                    FrameworkHint {
                        framework: "rails".into(),
                        role: "controller".into(),
                    },
                ));
            }
        }
    }
}

fn detect_laravel(symbols: &[RawSymbol], file_path: &str, hints: &mut Vec<(usize, FrameworkHint)>) {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if file_name.ends_with("Controller.php") {
        for (i, sym) in symbols.iter().enumerate() {
            if sym.kind == SymbolKind::Method {
                hints.push((
                    i,
                    FrameworkHint {
                        framework: "laravel".into(),
                        role: "controller".into(),
                    },
                ));
            }
        }
    }
}

fn detect_flutter(symbols: &[RawSymbol], hints: &mut Vec<(usize, FrameworkHint)>) {
    for (i, sym) in symbols.iter().enumerate() {
        if sym.signature.contains("StatelessWidget") || sym.signature.contains("StatefulWidget") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "flutter".into(),
                    role: "widget".into(),
                },
            ));
        }
    }
}

fn detect_swiftui(symbols: &[RawSymbol], hints: &mut Vec<(usize, FrameworkHint)>) {
    for (i, sym) in symbols.iter().enumerate() {
        if sym.signature.contains("some View") || sym.signature.contains(": View") {
            hints.push((
                i,
                FrameworkHint {
                    framework: "swiftui".into(),
                    role: "view".into(),
                },
            ));
        }
    }
}

fn detect_go_frameworks(symbols: &[RawSymbol], hints: &mut Vec<(usize, FrameworkHint)>) {
    for (i, sym) in symbols.iter().enumerate() {
        if sym.signature.contains("gin.")
            || sym.signature.contains("echo.")
            || sym.signature.contains("fiber.")
        {
            hints.push((
                i,
                FrameworkHint {
                    framework: "go-web".into(),
                    role: "handler".into(),
                },
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{SymbolKind, Visibility};

    fn make_symbol(name: &str, sig: &str) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 1,
            signature: sig.to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        }
    }

    fn make_method_symbol(name: &str, sig: &str) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Method,
            start_line: 1,
            end_line: 1,
            signature: sig.to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        }
    }

    #[test]
    fn detects_spring_controller() {
        let symbols = vec![make_symbol(
            "UserController",
            "@RestController public class UserController",
        )];
        let hints = detect_frameworks(&symbols, "src/UserController.java", "java");
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].1.framework, "spring");
        assert_eq!(hints[0].1.role, "controller");
    }

    #[test]
    fn detects_rails_controller() {
        let symbols = vec![make_method_symbol("index", "def index")];
        let hints = detect_frameworks(&symbols, "app/controllers/users_controller.rb", "ruby");
        assert!(!hints.is_empty());
        assert_eq!(hints[0].1.framework, "rails");
    }

    /// nw-160: only `app.get(`, `app.post(` and `router.` matched, so Fastify
    /// was entirely invisible and even Express PUT/DELETE/PATCH were missed.
    /// Across 40 repos that produced ONE HTTP contract in the whole org graph.
    #[test]
    fn http_route_detection_covers_fastify_and_every_express_verb() {
        // Previously matched — must keep matching.
        assert_eq!(http_route_framework("app.get('/a', h)"), Some("express"));
        assert_eq!(http_route_framework("app.post('/a', h)"), Some("express"));
        assert_eq!(http_route_framework("router.use(mw)"), Some("express"));

        // Express verbs that were silently missed.
        for verb in ["put", "delete", "patch", "head", "options", "all"] {
            assert_eq!(
                http_route_framework(&format!("app.{verb}('/a', h)")),
                Some("express"),
                "app.{verb}( must register as a route"
            );
        }

        // Fastify, which never matched at all.
        assert_eq!(
            http_route_framework("fastify.get('/a', h)"),
            Some("fastify")
        );
        assert_eq!(
            http_route_framework("fastify.route({ method: 'GET', url: '/a' })"),
            Some("fastify")
        );

        // Not a route registration.
        assert_eq!(http_route_framework("appointment.getTotal()"), None);
        assert_eq!(http_route_framework("const x = compute(app)"), None);
    }

    #[test]
    fn no_framework_for_plain_file() {
        let symbols = vec![make_symbol("Helper", "class Helper")];
        let hints = detect_frameworks(&symbols, "src/helper.java", "java");
        assert!(hints.is_empty());
    }
}
