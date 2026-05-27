use nestweaver_schema::Language;
use std::path::Path;

/// Detect the programming language from a file's extension.
/// Returns `None` for unsupported or missing extensions.
///
/// Markdown is NOT a code language and is detected separately via
/// [`is_markdown`] — keeping `Language` strictly code-typed avoids
/// breaking the exhaustive matches in the code resolver and confidence
/// scoring.
pub fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "js" => Some(Language::JavaScript),
        "ts" | "tsx" => Some(Language::TypeScript),
        "java" => Some(Language::Java),
        "go" => Some(Language::Go),
        "py" => Some(Language::Python),
        "cpp" | "cc" | "cxx" | "hpp" => Some(Language::Cpp),
        "rs" => Some(Language::Rust),
        "kt" | "kts" => Some(Language::Kotlin),
        "cs" => Some(Language::CSharp),
        "php" => Some(Language::Php),
        "rb" | "rake" => Some(Language::Ruby),
        "swift" => Some(Language::Swift),
        "c" | "h" => Some(Language::C),
        "dart" => Some(Language::Dart),
        "cbl" | "cob" | "cpy" => Some(Language::Cobol),
        "lua" => Some(Language::Lua),
        "sh" | "bash" => Some(Language::Bash),
        "scala" | "sc" => Some(Language::Scala),
        "ex" | "exs" => Some(Language::Elixir),
        "zig" => Some(Language::Zig),
        "m" | "mm" => Some(Language::ObjectiveC),
        "groovy" | "gradle" => Some(Language::Groovy),
        "ps1" | "psm1" => Some(Language::PowerShell),
        "jl" => Some(Language::Julia),
        "sql" => Some(Language::Sql),
        "tf" | "hcl" => Some(Language::Hcl),
        "f90" | "f95" | "f03" | "f08" => Some(Language::Fortran),
        "pas" | "pp" => Some(Language::Pascal),
        "vue" => Some(Language::Vue),
        "svelte" => Some(Language::Svelte),
        "astro" => Some(Language::Astro),
        "sv" | "svh" => Some(Language::SystemVerilog),
        _ => None,
    }
}

/// True if `path` looks like a Markdown file (`.md` or `.markdown`).
pub fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detect_language_js() {
        assert_eq!(
            detect_language(Path::new("foo.js")),
            Some(Language::JavaScript)
        );
    }

    #[test]
    fn detect_language_ts() {
        assert_eq!(
            detect_language(Path::new("foo.ts")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn detect_language_tsx() {
        assert_eq!(
            detect_language(Path::new("foo.tsx")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn detect_language_java() {
        assert_eq!(detect_language(Path::new("Foo.java")), Some(Language::Java));
    }

    #[test]
    fn detect_language_go() {
        assert_eq!(detect_language(Path::new("foo.go")), Some(Language::Go));
    }

    #[test]
    fn detect_language_python() {
        assert_eq!(detect_language(Path::new("foo.py")), Some(Language::Python));
    }

    #[test]
    fn detect_language_unsupported() {
        assert_eq!(detect_language(Path::new("foo.wat")), None);
    }

    #[test]
    fn detect_language_zig() {
        assert_eq!(detect_language(Path::new("main.zig")), Some(Language::Zig));
    }

    #[test]
    fn detect_language_objectivec() {
        assert_eq!(
            detect_language(Path::new("Foo.m")),
            Some(Language::ObjectiveC)
        );
    }

    #[test]
    fn detect_language_objectivec_mm() {
        assert_eq!(
            detect_language(Path::new("Foo.mm")),
            Some(Language::ObjectiveC)
        );
    }

    #[test]
    fn detect_language_groovy() {
        assert_eq!(
            detect_language(Path::new("Build.groovy")),
            Some(Language::Groovy)
        );
    }

    #[test]
    fn detect_language_gradle() {
        assert_eq!(
            detect_language(Path::new("build.gradle")),
            Some(Language::Groovy)
        );
    }

    #[test]
    fn detect_language_powershell() {
        assert_eq!(
            detect_language(Path::new("script.ps1")),
            Some(Language::PowerShell)
        );
    }

    #[test]
    fn detect_language_powershell_module() {
        assert_eq!(
            detect_language(Path::new("module.psm1")),
            Some(Language::PowerShell)
        );
    }

    #[test]
    fn detect_language_no_extension() {
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }

    #[test]
    fn detect_language_kotlin() {
        assert_eq!(detect_language(Path::new("Foo.kt")), Some(Language::Kotlin));
    }

    #[test]
    fn detect_language_kotlin_script() {
        assert_eq!(
            detect_language(Path::new("build.kts")),
            Some(Language::Kotlin)
        );
    }

    #[test]
    fn detect_language_csharp() {
        assert_eq!(detect_language(Path::new("Foo.cs")), Some(Language::CSharp));
    }

    #[test]
    fn detect_language_php() {
        assert_eq!(detect_language(Path::new("index.php")), Some(Language::Php));
    }

    #[test]
    fn detect_language_ruby() {
        assert_eq!(detect_language(Path::new("app.rb")), Some(Language::Ruby));
    }

    #[test]
    fn detect_language_ruby_rake() {
        assert_eq!(
            detect_language(Path::new("task.rake")),
            Some(Language::Ruby)
        );
    }

    #[test]
    fn detect_language_swift() {
        assert_eq!(
            detect_language(Path::new("main.swift")),
            Some(Language::Swift)
        );
    }

    #[test]
    fn detect_language_c() {
        assert_eq!(detect_language(Path::new("main.c")), Some(Language::C));
    }

    #[test]
    fn detect_language_c_header() {
        assert_eq!(detect_language(Path::new("header.h")), Some(Language::C));
    }

    #[test]
    fn detect_language_dart() {
        assert_eq!(
            detect_language(Path::new("main.dart")),
            Some(Language::Dart)
        );
    }

    #[test]
    fn detect_language_cobol() {
        assert_eq!(
            detect_language(Path::new("prog.cbl")),
            Some(Language::Cobol)
        );
    }

    #[test]
    fn detect_language_cobol_cob() {
        assert_eq!(
            detect_language(Path::new("prog.cob")),
            Some(Language::Cobol)
        );
    }

    #[test]
    fn detect_language_cobol_copybook() {
        assert_eq!(
            detect_language(Path::new("copy.cpy")),
            Some(Language::Cobol)
        );
    }

    #[test]
    fn detect_language_hpp_still_cpp() {
        assert_eq!(detect_language(Path::new("foo.hpp")), Some(Language::Cpp));
    }

    #[test]
    fn detect_language_lua() {
        assert_eq!(detect_language(Path::new("init.lua")), Some(Language::Lua));
    }

    #[test]
    fn detect_language_bash_sh() {
        assert_eq!(detect_language(Path::new("run.sh")), Some(Language::Bash));
    }

    #[test]
    fn detect_language_bash_bash() {
        assert_eq!(detect_language(Path::new("run.bash")), Some(Language::Bash));
    }

    #[test]
    fn detect_language_scala() {
        assert_eq!(
            detect_language(Path::new("Main.scala")),
            Some(Language::Scala)
        );
    }

    #[test]
    fn detect_language_scala_sc() {
        assert_eq!(
            detect_language(Path::new("script.sc")),
            Some(Language::Scala)
        );
    }

    #[test]
    fn detect_language_elixir_ex() {
        assert_eq!(detect_language(Path::new("lib.ex")), Some(Language::Elixir));
    }

    #[test]
    fn detect_language_elixir_exs() {
        assert_eq!(
            detect_language(Path::new("test.exs")),
            Some(Language::Elixir)
        );
    }

    #[test]
    fn detect_language_julia() {
        assert_eq!(detect_language(Path::new("main.jl")), Some(Language::Julia));
    }

    #[test]
    fn detect_language_sql() {
        assert_eq!(
            detect_language(Path::new("schema.sql")),
            Some(Language::Sql)
        );
    }

    #[test]
    fn detect_language_hcl_tf() {
        assert_eq!(detect_language(Path::new("main.tf")), Some(Language::Hcl));
    }

    #[test]
    fn detect_language_hcl_ext() {
        assert_eq!(
            detect_language(Path::new("config.hcl")),
            Some(Language::Hcl)
        );
    }

    #[test]
    fn detect_language_fortran_f90() {
        assert_eq!(
            detect_language(Path::new("module.f90")),
            Some(Language::Fortran)
        );
    }

    #[test]
    fn detect_language_fortran_f95() {
        assert_eq!(
            detect_language(Path::new("module.f95")),
            Some(Language::Fortran)
        );
    }

    #[test]
    fn detect_language_fortran_f03() {
        assert_eq!(
            detect_language(Path::new("module.f03")),
            Some(Language::Fortran)
        );
    }

    #[test]
    fn detect_language_fortran_f08() {
        assert_eq!(
            detect_language(Path::new("module.f08")),
            Some(Language::Fortran)
        );
    }

    #[test]
    fn detect_language_pascal_pas() {
        assert_eq!(
            detect_language(Path::new("unit.pas")),
            Some(Language::Pascal)
        );
    }

    #[test]
    fn detect_language_pascal_pp() {
        assert_eq!(
            detect_language(Path::new("unit.pp")),
            Some(Language::Pascal)
        );
    }

    #[test]
    fn detect_language_vue() {
        assert_eq!(detect_language(Path::new("App.vue")), Some(Language::Vue));
    }

    #[test]
    fn detect_language_svelte() {
        assert_eq!(
            detect_language(Path::new("App.svelte")),
            Some(Language::Svelte)
        );
    }

    #[test]
    fn detect_language_astro() {
        assert_eq!(
            detect_language(Path::new("Page.astro")),
            Some(Language::Astro)
        );
    }

    #[test]
    fn detect_language_systemverilog() {
        assert_eq!(
            detect_language(Path::new("module.sv")),
            Some(Language::SystemVerilog)
        );
    }

    #[test]
    fn detect_language_systemverilog_header() {
        assert_eq!(
            detect_language(Path::new("defs.svh")),
            Some(Language::SystemVerilog)
        );
    }
}
