//! Per-language import resolution, end to end: parse real sources, resolve,
//! and check the CALLS edges. Each case mirrors a QA reproduction repository.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use nestweaver_parser::{RawReference, RawSymbol, parse_source};
use nestweaver_schema::{EdgeType, Language, ResolvedEdge, symbol_uid};

use crate::resolve::resolve_references_with_file_languages;
use crate::workspace::{GoModule, WorkspaceContext};

const REPO: &str = "repo:test:imports";

type Files = Vec<(String, Vec<RawSymbol>, Vec<RawReference>)>;

#[test]
fn php_functions_use_repo_namespace_identity_and_refuse_ambiguity() {
    let inputs = [
        (
            "inc/global.php",
            "<?php\nfunction fallback() {}\nfunction util_fn() {}\nfunction duplicate() {}\n",
        ),
        (
            "lib/helpers.php",
            "<?php\nnamespace App;\nfunction helper() {}\nfunction fallback() {}\n",
        ),
        (
            "lib/vendor.php",
            "<?php\nnamespace Vendor;\nfunction helper() {}\n",
        ),
        ("other/duplicate.php", "<?php\nfunction duplicate() {}\n"),
        (
            "app/main.php",
            "<?php\nnamespace App;\nuse function Vendor\\helper as imported;\nuse Vendor as Alias;\nuse Vendor\\helper as classOnly;\nfunction run() {\nhelper();\nfallback();\nimported();\n\\util_fn();\nnamespace\\helper();\nAlias\\helper();\nclassOnly();\nduplicate();\n}\n",
        ),
        (
            "entry.php",
            "<?php\nrequire_once 'inc/global.php';\nutil_fn();\n",
        ),
    ];
    let (files, edges) = resolve(&inputs, Language::Php, &WorkspaceContext::default());
    let actual = calls(&files, &edges);
    let expected = BTreeSet::from([
        "app/main.php::run -> lib/helpers.php::helper".to_string(),
        "app/main.php::run -> lib/helpers.php::fallback".to_string(),
        "app/main.php::run -> lib/vendor.php::helper".to_string(),
        "app/main.php::run -> inc/global.php::util_fn".to_string(),
        "entry.php::<module> -> inc/global.php::util_fn".to_string(),
    ]);
    assert_eq!(actual, expected);
    assert!(
        edges
            .iter()
            .any(|e| e.target_uid == "unresolved:classOnly" && e.confidence == 0.0)
    );
    assert!(
        edges
            .iter()
            .any(|e| e.target_uid == "unresolved:duplicate" && e.confidence == 0.0)
    );
    let helper_uid = symbol_uid(REPO, "lib/helpers.php", "helper", 3);
    assert!(edges.iter().any(
        |e| e.source_uid == symbol_uid(REPO, "app/main.php", "run", 6)
            && e.target_uid == helper_uid
    ));
}

#[test]
fn php_braced_and_semicolon_regions_keep_same_file_homonyms_separate() {
    for source in [
        "<?php\nnamespace A {\nfunction x() {}\nfunction first() { x(); }\n}\nnamespace B {\nfunction x() {}\nfunction second() { x(); }\n}\n",
        "<?php\nnamespace A;\nfunction x() {}\nfunction first() { x(); }\nnamespace B;\nfunction x() {}\nfunction second() { x(); }\n",
    ] {
        let (files, edges) = resolve(
            &[("regions.php", source)],
            Language::Php,
            &WorkspaceContext::default(),
        );
        let symbols = &files[0].1;
        for (caller, namespace) in [("first", "A"), ("second", "B")] {
            let caller = symbols.iter().find(|s| s.name == caller).unwrap();
            let target = symbols
                .iter()
                .find(|s| s.name == "x" && s.scope_chain.as_deref() == Some(namespace))
                .unwrap();
            assert!(edges.iter().any(|e| e.source_uid
                == symbol_uid(REPO, "regions.php", &caller.name, caller.start_line)
                && e.target_uid
                    == symbol_uid(REPO, "regions.php", &target.name, target.start_line)));
        }
        assert_eq!(
            edges
                .iter()
                .filter(|e| e.edge_type == EdgeType::Calls && e.confidence > 0.0)
                .count(),
            2
        );
    }
}

#[test]
fn php_imports_are_region_scoped_and_names_are_ascii_case_insensitive() {
    let input = [
        (
            "definitions.php",
            "<?php\nnamespace Vendor;\nfunction helper() {}\nfunction second() {}\n",
        ),
        ("global.php", "<?php\nfunction helper() {}\n"),
        (
            "app.php",
            "<?php\nnamespace A {\nuse function Vendor\\{helper as One, second};\nfunction first() { ONE(); second(); }\n}\nnamespace B {\nuse Vendor\\{function helper as Two, const VALUE, Other};\nfunction next() { Two(); One(); helper(); }\n}\nnamespace {\nfunction last() { HeLpEr(); }\n}\n",
        ),
    ];
    let (files, edges) = resolve(&input, Language::Php, &WorkspaceContext::default());
    assert_eq!(
        calls(&files, &edges),
        BTreeSet::from([
            "app.php::first -> definitions.php::helper".into(),
            "app.php::first -> definitions.php::second".into(),
            "app.php::next -> definitions.php::helper".into(),
            "app.php::next -> global.php::helper".into(),
            "app.php::last -> global.php::helper".into(),
        ])
    );
    assert!(edges.iter().any(|e| e.target_uid == "unresolved:One"));
}

#[test]
fn php_function_lookup_never_borrows_a_method_or_foreign_language_function() {
    let input = [
        (
            "app.php",
            "<?php\nclass Worker {\nfunction helper() {}\nfunction run() { $this->helper(); }\n}\nfunction main() { helper(); js_only(); new Worker(); }\n",
        ),
        ("external.js", "function js_only() {}"),
    ];
    let (files, edges) = resolve(&input, Language::Php, &WorkspaceContext::default());
    let actual = calls(&files, &edges);
    assert!(actual.contains("app.php::run -> app.php::helper"));
    assert!(actual.contains("app.php::main -> app.php::Worker"));
    assert!(!actual.contains("app.php::main -> app.php::helper"));
    assert!(!actual.contains("app.php::main -> external.js::js_only"));
}

#[test]
fn php_unknown_stored_namespace_metadata_refuses_global_borrowing() {
    let input = [
        (
            "app.php",
            "<?php\nnamespace App;\nfunction run() { helper(); }\n",
        ),
        (
            "unknown.php",
            "<?php\nnamespace App;\nfunction helper() {}\n",
        ),
        ("global.php", "<?php\nfunction helper() {}\n"),
    ];
    let (mut files, _) = resolve(&input, Language::Php, &WorkspaceContext::default());
    for symbol in &mut files[1].1 {
        symbol.scope_chain = None;
    }
    files[1].2.clear();
    let edges = crate::resolve_references(&files, Language::Php, REPO);
    assert!(
        calls(&files, &edges).is_empty(),
        "an unqualified stored PHP function must not be assumed global"
    );
    assert!(edges.iter().any(|e| e.target_uid == "unresolved:helper"));
}

#[test]
fn php_chained_member_calls_never_borrow_free_function_homonyms() {
    for global_run in [false, true] {
        let suffix = if global_run {
            "function run() {}\n"
        } else {
            ""
        };
        let source = format!(
            "<?php\nclass Worker {{\n public function run() {{}}\n public function factory() {{}}\n public function typedControl() {{ $this->run(); }}\n}}\nfunction factory(): Worker {{ return new Worker(); }}\nfunction chainedRun() {{ factory()->run(); }}\nfunction chainedSameName() {{ factory()->factory(); }}\n{suffix}"
        );
        let (files, edges) = resolve_typed(&[("chains.php", &source)], Language::Php);
        let symbols = &files[0].1;
        let uid = |name: &str, kind| {
            let matches: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == name && s.kind == kind)
                .collect();
            assert_eq!(matches.len(), 1);
            symbol_uid(REPO, "chains.php", name, matches[0].start_line)
        };
        for (caller, method) in [("chainedRun", "run"), ("chainedSameName", "factory")] {
            let actual: BTreeSet<_> = edges
                .iter()
                .filter(|e| {
                    e.edge_type == EdgeType::Calls
                        && e.confidence > 0.0
                        && e.source_uid == uid(caller, nestweaver_schema::SymbolKind::Function)
                })
                .map(|e| e.target_uid.clone())
                .collect();
            // PHP return annotations are not extracted today. The existing
            // member resolver therefore refuses an untyped factory chain;
            // this repair must preserve that refusal instead of borrowing a
            // free homonym. A separately typed self control remains resolvable.
            assert_eq!(
                actual,
                BTreeSet::from([uid("factory", nestweaver_schema::SymbolKind::Function),]),
                "{caller}/{method}, global run={global_run}"
            );
        }
        let typed = uid("typedControl", nestweaver_schema::SymbolKind::Method);
        let target = uid("run", nestweaver_schema::SymbolKind::Method);
        assert!(edges.iter().any(|e| e.source_uid == typed
            && e.target_uid == target
            && e.edge_type == EdgeType::Calls
            && e.confidence > 0.0));
    }
}

#[test]
fn php_constructor_and_parameter_receivers_keep_unscoped_type_evidence() {
    let source = "<?php\nclass Earlier { public function deliver() {} }\nclass Intended { public function deliver() {} }\nfunction constructed() { $bound = new Intended(); $bound->deliver(); }\nfunction parameter(Intended $bound) { $bound->deliver(); }\nfunction deliver() {}\n";
    let (files, edges) = resolve_typed(&[("receivers.php", source)], Language::Php);
    let expected = BTreeSet::from([
        (
            symbol_uid(REPO, "receivers.php", "constructed", 4),
            symbol_uid(REPO, "receivers.php", "Intended", 3),
        ),
        (
            symbol_uid(REPO, "receivers.php", "constructed", 4),
            symbol_uid(REPO, "receivers.php", "deliver", 3),
        ),
        (
            symbol_uid(REPO, "receivers.php", "parameter", 5),
            symbol_uid(REPO, "receivers.php", "deliver", 3),
        ),
    ]);
    let actual: BTreeSet<_> = edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Calls && e.confidence > 0.0)
        .map(|e| (e.source_uid.clone(), e.target_uid.clone()))
        .collect();
    assert_eq!(
        actual, expected,
        "must not borrow Earlier::deliver or global deliver"
    );
    assert!(
        files[0]
            .2
            .iter()
            .filter(|r| r.kind == nestweaver_parser::ReferenceKind::Call
                && (r.receiver.is_some() || r.name == "Intended"))
            .all(|r| r.scope.is_none())
    );
}

fn resolve(
    inputs: &[(&str, &str)],
    language: Language,
    workspace: &WorkspaceContext,
) -> (Files, Vec<ResolvedEdge>) {
    let mut files = Vec::new();
    let mut languages = HashMap::new();
    for (path, source) in inputs {
        let parsed = parse_source(Path::new(path), source).unwrap();
        let detected = nestweaver_parser::detect_language(Path::new(path)).unwrap();
        languages.insert((*path).to_string(), detected);
        files.push(((*path).to_string(), parsed.symbols, parsed.references));
    }
    let edges = resolve_references_with_file_languages(
        &files,
        language,
        REPO,
        workspace,
        None,
        None,
        Some(&languages),
    );
    (files, edges)
}

/// `(caller, callee)` names as `file::name`, for every CALLS edge.
fn calls(files: &Files, edges: &[ResolvedEdge]) -> BTreeSet<String> {
    let mut names = HashMap::new();
    for (file, symbols, _) in files {
        for symbol in symbols {
            names.insert(
                symbol_uid(REPO, file, &symbol.name, symbol.start_line),
                format!("{file}::{}", symbol.name),
            );
        }
    }
    edges
        .iter()
        .filter(|edge| edge.edge_type == EdgeType::Calls)
        .filter_map(|edge| {
            Some(format!(
                "{} -> {}",
                names.get(&edge.source_uid)?,
                names.get(&edge.target_uid)?
            ))
        })
        .collect()
}

fn go_workspace() -> WorkspaceContext {
    WorkspaceContext {
        go_modules: vec![GoModule {
            path: "example.com/go1".into(),
            directory: String::new(),
        }],
        ..Default::default()
    }
}

/// QA go1: a real module (`module example.com/go1`) whose `cmd/app` calls
/// into `internal/calc`, with the package split over two files and imported
/// both plainly and under an alias.
#[test]
fn go_module_import_reaches_every_file_of_the_package() {
    let main = r#"package main

import (
	"fmt"
	c "example.com/go1/internal/calc"
)

func main() {
	fmt.Println(c.New(2), c.Sum(1.0, 2.0))
}
"#;
    let other = r#"package main

import "example.com/go1/internal/calc"

func other() int {
	return calc.Sum(1, 2) + calc.Twice(3)
}

func bare() int {
	return Twice(1)
}
"#;
    let calc = r#"package calc

type Impl struct{ base int }

func New(b int) *Impl { return &Impl{base: b} }

func Sum(xs ...int) int { return 0 }
"#;
    let twice = r#"package calc

func Twice(x int) int { return 2 * x }
"#;
    let test = r#"package calc

func TestTwice() { Twice(1) }
"#;
    let inputs = [
        ("cmd/app/main.go", main),
        ("cmd/app/other.go", other),
        ("internal/calc/calc.go", calc),
        ("internal/calc/twice.go", twice),
        ("internal/calc/twice_test.go", test),
    ];
    let (files, edges) = resolve(&inputs, Language::Go, &go_workspace());
    let calls = calls(&files, &edges);
    for expected in [
        "cmd/app/main.go::main -> internal/calc/calc.go::New",
        "cmd/app/main.go::main -> internal/calc/calc.go::Sum",
        "cmd/app/other.go::other -> internal/calc/calc.go::Sum",
        "cmd/app/other.go::other -> internal/calc/twice.go::Twice",
        "internal/calc/twice_test.go::TestTwice -> internal/calc/twice.go::Twice",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
    // Counterweight: a bare `Twice` in package main is not the imported
    // package's `Twice`; Go requires the qualifier.
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("cmd/app/other.go::bare ->")),
        "{calls:#?}"
    );
}

/// Without the module path the import path never matched a directory.
#[test]
fn go_module_import_needs_the_module_path() {
    let main =
        "package main\n\nimport \"example.com/go1/internal/calc\"\n\nfunc main() { calc.New() }\n";
    let calc = "package calc\n\nfunc New() int { return 1 }\n";
    let inputs = [("cmd/app/main.go", main), ("internal/calc/lib.go", calc)];
    let (files, edges) = resolve(&inputs, Language::Go, &go_workspace());
    assert!(
        calls(&files, &edges).contains("cmd/app/main.go::main -> internal/calc/lib.go::New"),
        "{:#?}",
        calls(&files, &edges)
    );
}

/// QA py2: every way of binding a submodule reaches its functions.
#[test]
fn python_submodule_imports_bind_the_module() {
    let user = r#"from . import helpers
from . import helpers as hh
from .helpers import h3 as three
import pkg.helpers
import pkg.helpers as ph

def a():
    return helpers.h1()
def b():
    return hh.h2()
def c():
    return three()
def d():
    return pkg.helpers.h4()
def e():
    return ph.h5()
"#;
    let helpers = "def h1():\n    return 1\ndef h2():\n    return 2\ndef h3():\n    return 3\ndef h4():\n    return 4\ndef h5():\n    return 5\n";
    let inputs = [
        ("pkg/__init__.py", ""),
        ("pkg/helpers.py", helpers),
        ("pkg/user.py", user),
    ];
    let (files, edges) = resolve(&inputs, Language::Python, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    for (caller, callee) in [
        ("a", "h1"),
        ("b", "h2"),
        ("c", "h3"),
        ("d", "h4"),
        ("e", "h5"),
    ] {
        let expected = format!("pkg/user.py::{caller} -> pkg/helpers.py::{callee}");
        assert!(calls.contains(&expected), "missing {expected}: {calls:#?}");
    }
}

/// QA py1: `from .sub import helpers as h` binds the submodule, and a name
/// a package re-exports under an alias reaches its real definition.
#[test]
fn python_package_reexport_alias_and_submodule_binding() {
    let init = "from .core import compute as compute_alias\nfrom .sub.helpers import helper\n";
    let core = r#"from .sub import helpers as h


def compute(x):
    return h.helper(x)
"#;
    let helpers = "def helper(x):\n    return x + 1\n";
    let test = r#"from pkg import compute_alias


def test_compute():
    assert compute_alias(1) == 2
"#;
    let inputs = [
        ("pkg/__init__.py", init),
        ("pkg/core.py", core),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/helpers.py", helpers),
        ("tests/test_core.py", test),
    ];
    let (files, edges) = resolve(&inputs, Language::Python, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    for expected in [
        "pkg/core.py::compute -> pkg/sub/helpers.py::helper",
        "tests/test_core.py::test_compute -> pkg/core.py::compute",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
}

/// Counterweight: a name the package itself defines is the attribute, not a
/// same-named submodule (Python reads the attribute first).
#[test]
fn python_package_attribute_wins_over_submodule() {
    let init = "def helpers():\n    return 0\n";
    let user = "from pkg import helpers\n\n\ndef run():\n    return helpers()\n";
    let inputs = [
        ("pkg/__init__.py", init),
        ("pkg/helpers.py", "def helpers():\n    return 1\n"),
        ("app/user.py", user),
    ];
    let (files, edges) = resolve(&inputs, Language::Python, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    assert!(
        calls.contains("app/user.py::run -> pkg/__init__.py::helpers"),
        "{calls:#?}"
    );
}

/// Counterweight: a package that re-binds a submodule's name by import
/// (`from .helpers import helpers` in `__init__.py`) exposes that binding,
/// not the submodule, so the client's `helpers()` is the function.
#[test]
fn python_package_import_binding_wins_over_submodule() {
    let init = "from .helpers import helpers\n";
    let helpers = "def helpers():\n    return 1\n";
    let user = "from pkg import helpers\n\n\ndef run():\n    return helpers()\n";
    let inputs = [
        ("pkg/__init__.py", init),
        ("pkg/helpers.py", helpers),
        ("app/user.py", user),
    ];
    let (files, edges) = resolve(&inputs, Language::Python, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    assert!(
        calls.contains("app/user.py::run -> pkg/helpers.py::helpers"),
        "{calls:#?}"
    );
}

/// QA kt1: Kotlin imports name declarations of a package, not files. Here no
/// file is named after any imported symbol.
#[test]
fn kotlin_imports_resolve_by_package_declaration() {
    let things = r#"package com.acme.a

fun helper(x: Int): Int = x + 1

fun Int.doubled(): Int = this * 2
"#;
    let main = r#"package com.acme.b

import com.acme.a.helper
import com.acme.a.doubled

fun main() {
    println(helper(2) + 3.doubled())
}
"#;
    let wildcard = r#"package com.acme.c

import com.acme.a.*

fun run() = helper(1)
"#;
    // An unconventional directory: the package clause alone places it.
    let elsewhere = "package com.acme.a\n\nfun tripled(x: Int): Int = 3 * x\n";
    let user = "package com.acme.d\n\nimport com.acme.a.tripled\n\nfun go() = tripled(1)\n";
    let inputs = [
        ("src/main/kotlin/com/acme/a/Things.kt", things),
        ("src/main/kotlin/com/acme/b/Main.kt", main),
        ("src/main/kotlin/com/acme/c/Run.kt", wildcard),
        ("src/main/kotlin/misc/Elsewhere.kt", elsewhere),
        ("src/main/kotlin/com/acme/d/User.kt", user),
    ];
    let (files, edges) = resolve(&inputs, Language::Kotlin, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    for expected in [
        "src/main/kotlin/com/acme/b/Main.kt::main -> src/main/kotlin/com/acme/a/Things.kt::helper",
        "src/main/kotlin/com/acme/b/Main.kt::main -> src/main/kotlin/com/acme/a/Things.kt::doubled",
        "src/main/kotlin/com/acme/c/Run.kt::run -> src/main/kotlin/com/acme/a/Things.kt::helper",
        "src/main/kotlin/com/acme/d/User.kt::go -> src/main/kotlin/misc/Elsewhere.kt::tripled",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
}

/// Java: a package-private top-level class need not live in a file named
/// after it, and a nested class import reaches its outer class's file.
#[test]
fn java_imports_resolve_by_package_declaration() {
    let shapes = r#"package com.acme.geo;

public class Shapes {
    public static class Circle {
        public static int area() { return 3; }
    }
}

class Helper {
    static int assist() { return 1; }
}
"#;
    let main = r#"package com.acme.app;

import com.acme.geo.Helper;
import com.acme.geo.Shapes.Circle;

public class Main {
    public static void main(String[] args) {
        Helper.assist();
        Circle.area();
    }
}
"#;
    let inputs = [
        ("src/main/java/com/acme/geo/Shapes.java", shapes),
        ("src/main/java/com/acme/app/Main.java", main),
    ];
    let (files, edges) = resolve(&inputs, Language::Java, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    for expected in [
        "src/main/java/com/acme/app/Main.java::main -> src/main/java/com/acme/geo/Shapes.java::assist",
        "src/main/java/com/acme/app/Main.java::main -> src/main/java/com/acme/geo/Shapes.java::area",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
}

/// The parser facts the import resolution above relies on.
#[test]
fn parser_records_import_names_and_package_clauses() {
    use nestweaver_parser::ReferenceKind;
    let go = parse_source(
        Path::new("main.go"),
        "package main\n\nimport (\n\tc \"example.com/x/calc\"\n\t. \"example.com/x/dot\"\n\t_ \"example.com/x/side\"\n\t\"example.com/x/plain\"\n)\n",
    )
    .unwrap();
    let mut names: Vec<(String, Option<String>)> = go
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Import)
        .map(|reference| (reference.name.clone(), reference.receiver.clone()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("example.com/x/calc".into(), Some("c".into())),
            ("example.com/x/dot".into(), Some(".".into())),
            ("example.com/x/plain".into(), None),
            ("example.com/x/side".into(), Some("_".into())),
        ]
    );

    for (path, source, package) in [
        ("A.kt", "package com.acme.a\n\nfun f() = 1\n", "com.acme.a"),
        (
            "A.java",
            "package com.acme.a;\n\nclass A {}\n",
            "com.acme.a",
        ),
    ] {
        let parsed = parse_source(Path::new(path), source).unwrap();
        let packages: Vec<&str> = parsed
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::PackageBinding)
            .map(|reference| reference.name.as_str())
            .collect();
        assert_eq!(packages, vec![package], "{path}");
    }

    let python = parse_source(Path::new("m.py"), "import pkg.helpers as ph\n").unwrap();
    assert!(
        python
            .references
            .iter()
            .any(|r| r.kind == ReferenceKind::Import && r.name == "pkg.helpers")
    );
    assert!(python.references.iter().any(|r| {
        r.kind == ReferenceKind::ImportAlias
            && r.name == "ph"
            && r.context == "pkg.helpers"
            && r.receiver.as_deref() == Some("*")
    }));
}

/// [`resolve`] with per-file type environments, as the indexer passes them:
/// typed receivers (`Foo f = new Foo(); f.hit();`) need them.
fn resolve_typed(inputs: &[(&str, &str)], language: Language) -> (Files, Vec<ResolvedEdge>) {
    let mut files = Vec::new();
    let mut languages = HashMap::new();
    let mut envs = HashMap::new();
    for (path, source) in inputs {
        let parsed = parse_source(Path::new(path), source).unwrap();
        let detected = nestweaver_parser::detect_language(Path::new(path)).unwrap();
        languages.insert((*path).to_string(), detected);
        envs.insert(
            (*path).to_string(),
            crate::types::TypeEnvironment::build(
                source,
                detected,
                &parsed.symbols,
                &parsed.type_bindings,
            ),
        );
        files.push(((*path).to_string(), parsed.symbols, parsed.references));
    }
    let edges = resolve_references_with_file_languages(
        &files,
        language,
        REPO,
        &WorkspaceContext::default(),
        Some(&envs),
        None,
        Some(&languages),
    );
    (files, edges)
}

/// QA (post-#515): Kotlin name precedence is explicit import, then same
/// package, then star import. `import com.k1.make` imports `make`, not every top-level
/// declaration of its file, so `Thing()` in `com.k2` is `com.k2.Thing`.
#[test]
fn kotlin_same_package_type_wins_over_package_imports() {
    let k1 = "package com.k1\n\nclass Thing\n\nfun make(): Thing = Thing()\n";
    let thing = "package com.k2\n\nclass Thing\n";
    let named =
        "package com.k2\n\nimport com.k1.make\n\nfun named() {\n    Thing()\n    make()\n}\n";
    let star = "package com.k2\n\nimport com.k1.*\n\nfun star() {\n    Thing()\n    make()\n}\n";
    let explicit = "package com.k2\n\nimport com.k1.Thing\n\nfun explicit() {\n    Thing()\n}\n";
    let inputs = [
        ("src/com/k1/K1.kt", k1),
        ("src/com/k2/Thing.kt", thing),
        ("src/com/k2/Use.kt", named),
        ("src/com/k2/Star.kt", star),
        ("src/com/k2/Explicit.kt", explicit),
    ];
    let (files, edges) = resolve(&inputs, Language::Kotlin, &WorkspaceContext::default());
    let calls = calls(&files, &edges);
    for expected in [
        "src/com/k2/Use.kt::named -> src/com/k2/Thing.kt::Thing",
        "src/com/k2/Use.kt::named -> src/com/k1/K1.kt::make",
        "src/com/k2/Star.kt::star -> src/com/k2/Thing.kt::Thing",
        "src/com/k2/Star.kt::star -> src/com/k1/K1.kt::make",
        "src/com/k2/Explicit.kt::explicit -> src/com/k1/K1.kt::Thing",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
    for unexpected in [
        "src/com/k2/Use.kt::named -> src/com/k1/K1.kt::Thing",
        "src/com/k2/Star.kt::star -> src/com/k1/K1.kt::Thing",
        "src/com/k2/Explicit.kt::explicit -> src/com/k2/Thing.kt::Thing",
    ] {
        assert!(
            !calls.contains(unexpected),
            "unexpected {unexpected}: {calls:#?}"
        );
    }
}

/// QA (post-#515): in Java a same-package type shadows a type-import-on-
/// demand (JLS 6.4.1) and is shadowed by a single-type import. Covers the
/// constructed receiver (`new Foo().hit()`) and a local of declared type
/// (`Foo f = new Foo(); f.hit();`), with and without imports.
#[test]
fn java_same_package_type_wins_over_on_demand_import() {
    let foo_a = "package com.a;\n\npublic class Foo {\n    public void hit() {}\n}\n";
    let foo_b = "package com.b;\n\npublic class Foo {\n    public void hit() {}\n}\n";
    let on_demand = r#"package com.b;

import com.a.*;

public class Use {
    void direct() {
        new Foo().hit();
    }

    void local() {
        Foo f = new Foo();
        f.hit();
    }
}
"#;
    let plain = r#"package com.b;

public class Plain {
    void local() {
        Foo f = new Foo();
        f.hit();
    }
}
"#;
    let single = r#"package com.b;

import com.a.Foo;

public class Single {
    void local() {
        Foo f = new Foo();
        f.hit();
    }
}
"#;
    let inputs = [
        ("src/com/a/Foo.java", foo_a),
        ("src/com/b/Foo.java", foo_b),
        ("src/com/b/Use.java", on_demand),
        ("src/com/b/Plain.java", plain),
        ("src/com/b/Single.java", single),
    ];
    let (files, edges) = resolve_typed(&inputs, Language::Java);
    let calls = calls(&files, &edges);
    for expected in [
        "src/com/b/Use.java::direct -> src/com/b/Foo.java::hit",
        "src/com/b/Use.java::local -> src/com/b/Foo.java::hit",
        "src/com/b/Plain.java::local -> src/com/b/Foo.java::hit",
        "src/com/b/Single.java::local -> src/com/a/Foo.java::hit",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
    for unexpected in [
        "src/com/b/Use.java::direct -> src/com/a/Foo.java::hit",
        "src/com/b/Use.java::local -> src/com/a/Foo.java::hit",
        "src/com/b/Single.java::local -> src/com/b/Foo.java::hit",
    ] {
        assert!(
            !calls.contains(unexpected),
            "unexpected {unexpected}: {calls:#?}"
        );
    }
}

/// QA (post-#515): a `//go:build ignore` generator in a package directory is
/// a standalone program, not a file of the package. Its `Do` is neither the
/// imported callee nor a build variant of the real `Do`.
#[test]
fn go_build_ignore_file_is_not_a_package_callee() {
    let main =
        "package main\n\nimport \"example.com/go1/a/util\"\n\nfunc main() {\n\tutil.Do()\n}\n";
    let real = "package util\n\nfunc Do() {}\n";
    let other = "package util\n\nfunc Other() {\n\tDo()\n}\n";
    let generator = "//go:build ignore\n\npackage util\n\nfunc Do() {}\n";
    let inputs = [
        ("main.go", main),
        ("a/util/u.go", real),
        ("a/util/other.go", other),
        // Sorts first in its directory, as in the QA repro.
        ("a/util/gen.go", generator),
    ];
    let (files, edges) = resolve(&inputs, Language::Go, &go_workspace());
    let calls = calls(&files, &edges);
    for expected in [
        "main.go::main -> a/util/u.go::Do",
        "a/util/other.go::Other -> a/util/u.go::Do",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls:#?}");
    }
    assert!(
        !calls
            .iter()
            .any(|call| call.ends_with("-> a/util/gen.go::Do")),
        "a build-ignored file must not be a callee: {calls:#?}"
    );
}

/// Review finding: `import com.a.Outer;` brings `Outer` into scope, not its
/// nested `Inner`, so a bare `Inner` in package `com.b` is `com.b.Inner`.
#[test]
fn java_single_type_import_does_not_import_nested_types() {
    let outer = r#"package com.a;

public class Outer {
    public static class Inner {
        public void m() {}
    }
}
"#;
    let inner = "package com.b;\n\npublic class Inner {\n    public void m() {}\n}\n";
    let user = r#"package com.b;

import com.a.Outer;

public class User {
    void use() {
        Inner x = new Inner();
        x.m();
    }
}
"#;
    let inputs = [
        ("src/com/a/Outer.java", outer),
        ("src/com/b/Inner.java", inner),
        ("src/com/b/User.java", user),
    ];
    let (files, edges) = resolve_typed(&inputs, Language::Java);
    let calls = calls(&files, &edges);
    assert!(
        calls.contains("src/com/b/User.java::use -> src/com/b/Inner.java::m"),
        "{calls:#?}"
    );
    assert!(
        !calls.contains("src/com/b/User.java::use -> src/com/a/Outer.java::m"),
        "{calls:#?}"
    );
}
