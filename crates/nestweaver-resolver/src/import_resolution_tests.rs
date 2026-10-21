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
