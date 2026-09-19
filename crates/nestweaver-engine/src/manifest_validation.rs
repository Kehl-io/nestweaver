//! Fallible syntax admission for captured manifests. No source is evaluated.
use anyhow::{Context, Result, ensure};
use std::path::Path;

pub(super) fn validate(path: &Path, source: &str) -> Result<()> {
    ensure!(!source.contains('\0'), "manifest contains NUL bytes");
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    match name {
        "package.json" | "composer.json" => {
            let value: serde_json::Value = serde_json::from_str(source)?;
            ensure!(value.is_object(), "manifest JSON must be an object");
            if let Some(name) = value.get("name") {
                ensure!(name.is_string(), "manifest name must be a string");
            }
            for key in if name == "package.json" {
                &[
                    "dependencies",
                    "devDependencies",
                    "peerDependencies",
                    "optionalDependencies",
                ][..]
            } else {
                &["require", "require-dev"][..]
            } {
                if let Some(deps) = value.get(*key) {
                    let deps = deps
                        .as_object()
                        .with_context(|| format!("manifest {key} must be an object"))?;
                    ensure!(
                        deps.values().all(|v| v.is_string()),
                        "manifest {key} versions must be strings"
                    );
                }
            }
        }
        "Cargo.toml" | "pyproject.toml" => {
            let value: toml::Value = toml::from_str(source)?;
            let section = if name == "Cargo.toml" {
                "package"
            } else {
                "project"
            };
            if let Some(package) = value.get(section) {
                ensure!(package.is_table(), "manifest {section} must be a table");
                if let Some(name) = package.get("name") {
                    ensure!(name.is_str(), "manifest package name must be a string");
                }
                if section == "project"
                    && let Some(deps) = package.get("dependencies")
                {
                    ensure!(
                        deps.as_array()
                            .is_some_and(|a| a.iter().all(|v| v.is_str())),
                        "project dependencies must be an array of strings"
                    );
                }
            }
            if name == "Cargo.toml" {
                for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(deps) = value.get(key) {
                        ensure!(
                            deps.as_table()
                                .is_some_and(|t| t.values().all(|v| v.is_str() || v.is_table())),
                            "Cargo {key} must be a dependency table"
                        );
                    }
                }
            }
        }
        "pubspec.yaml" => {
            let value: serde_yaml::Value = serde_yaml::from_str(source)?;
            ensure!(value.is_mapping(), "manifest YAML must be a mapping");
            if let Some(name) = value.get("name") {
                ensure!(name.is_string(), "pubspec name must be a string");
            }
            for key in ["dependencies", "dev_dependencies"] {
                if let Some(deps) = value.get(key) {
                    ensure!(
                        deps.as_mapping()
                            .is_some_and(|m| m.keys().all(|k| k.is_string())),
                        "pubspec {key} must be a string-keyed mapping"
                    );
                }
            }
        }
        "go.mod" => {
            go_manifest(source)?;
        }
        "Gemfile" => {
            nestweaver_parser::parse::validate_manifest_syntax(Path::new("Gemfile.rb"), source)?
        }
        "Package.swift" | "build.gradle.kts" => {
            nestweaver_parser::parse::validate_manifest_syntax(path, source)?
        }
        "requirements.txt" => requirements(source)?,
        "CMakeLists.txt" => cmake(source)?,
        _ if path.extension().is_some_and(|e| e == "csproj") => {
            let document = roxmltree::Document::parse(source)?;
            ensure!(
                document.root_element().tag_name().name() == "Project",
                "csproj root must be Project"
            );
        }
        _ => anyhow::bail!("unsupported manifest syntax: {}", path.display()),
    }
    Ok(())
}

// Go module files have a directive grammar, not Go source grammar. Tokenize
// quoted arguments/comments before checking directive arity and block closure.
fn go_tokens(line: &str) -> Result<Vec<String>> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut tokens = Vec::new();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            break;
        }
        if bytes[i..].starts_with(b"=>") {
            tokens.push("=>".to_owned());
            i += 2;
            continue;
        }
        if b"()[],".contains(&bytes[i]) {
            tokens.push((bytes[i] as char).to_string());
            i += 1;
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'`' {
            let quote = bytes[i];
            i += 1;
            let mut value = Vec::new();
            while i < bytes.len() && bytes[i] != quote {
                // go.mod strings escape the next character; they do not use
                // Go source-language hexadecimal/octal escape semantics.
                if bytes[i] == b'\\' && quote == b'"' {
                    i += 1;
                    ensure!(i < bytes.len(), "unterminated Go quoted argument");
                }
                value.push(bytes[i]);
                i += 1;
            }
            ensure!(i < bytes.len(), "unterminated Go quoted argument");
            tokens.push(String::from_utf8(value)?);
            i += 1;
            ensure!(
                i == bytes.len()
                    || bytes[i].is_ascii_whitespace()
                    || b"),]".contains(&bytes[i])
                    || bytes[i..].starts_with(b"//")
                    || bytes[i..].starts_with(b"=>"),
                "invalid Go quoted argument suffix"
            );
        } else {
            let start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !b"()[],\"`".contains(&bytes[i])
                && !bytes[i..].starts_with(b"//")
                && !bytes[i..].starts_with(b"=>")
            {
                i += 1;
            }
            ensure!(i > start, "invalid Go argument");
            tokens.push(line[start..i].to_owned());
        }
    }
    Ok(tokens)
}
fn go_version(value: &str) -> bool {
    // Main-module go.mod files may use a noncanonical revision/branch token.
    // Resolving it would require network/toolchain execution and is not a
    // syntax-admission responsibility.
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-/".contains(&b))
}
fn go_module_path(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b))
}
pub(super) fn go_manifest(source: &str) -> Result<super::ManifestInfo> {
    let mut manifest = super::ManifestInfo::default();
    let mut block: Option<String> = None;
    let mut module_count = 0;
    for line in source.lines() {
        let tokens = go_tokens(line)?;
        if tokens.is_empty() {
            continue;
        }
        if tokens == [")"] {
            ensure!(block.take().is_some(), "unexpected Go directive block end");
            continue;
        }
        let (directive, args) = if let Some(block) = &block {
            (block.as_str(), tokens.as_slice())
        } else {
            (tokens[0].as_str(), &tokens[1..])
        };
        ensure!(
            matches!(
                directive,
                "module"
                    | "go"
                    | "toolchain"
                    | "require"
                    | "exclude"
                    | "replace"
                    | "retract"
                    | "godebug"
                    | "tool"
                    | "ignore"
            ),
            "unsupported Go module directive {directive}"
        );
        if args == ["("] {
            ensure!(block.is_none(), "invalid Go directive block");
            block = Some(directive.to_owned());
            continue;
        }
        ensure!(
            !args.iter().any(|a| a == "(" || a == ")"),
            "invalid Go directive delimiters"
        );
        let valid = match directive {
            "module" => {
                module_count += 1;
                args.len() == 1 && go_module_path(&args[0])
            }
            "go" => {
                args.len() == 1
                    && args[0].starts_with(|c: char| c.is_ascii_digit())
                    && args[0]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.')
            }
            "toolchain" => args.len() == 1 && (args[0] == "default" || args[0].starts_with("go1.")),
            "require" | "exclude" => {
                args.len() == 2 && go_module_path(&args[0]) && go_version(&args[1])
            }
            "replace" => args.iter().position(|s| s == "=>").is_some_and(|at| {
                (at == 1 || at == 2)
                    && (args.len() == at + 2 || args.len() == at + 3)
                    && (at == 1 || go_version(&args[1]))
                    && (args.len() == at + 2 || go_version(&args[at + 2]))
            }),
            "retract" => {
                (args.len() == 1 && go_version(&args[0]))
                    || (args.len() == 5
                        && args[0] == "["
                        && args[2] == ","
                        && args[4] == "]"
                        && go_version(&args[1])
                        && go_version(&args[3]))
            }
            "godebug" => {
                args.len() == 1
                    && args[0]
                        .split_once('=')
                        .is_some_and(|(k, v)| !k.is_empty() && !v.is_empty())
            }
            "tool" | "ignore" => args.len() == 1 && !args[0].is_empty(),
            _ => false,
        };
        ensure!(valid, "invalid Go {directive} directive");
        if directive == "module" {
            manifest.package_name = Some(args[0].clone());
        }
        if directive == "require" {
            manifest.dependencies.push(args[0].clone());
        }
    }
    ensure!(block.is_none(), "unterminated Go directive block");
    ensure!(
        module_count == 1,
        "Go manifest must contain one module directive"
    );
    Ok(manifest)
}

/// PEP 508 marker expression grammar. In particular ~= and === are package
/// comparison operators, so a programming-language expression parser is not
/// an interchangeable validator. Values are checked structurally, not evaluated.
fn requirement_marker(source: &str) -> Result<()> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            ensure!(i < bytes.len(), "unterminated requirement marker string");
            i += 1;
        } else if b"()".contains(&bytes[i]) {
            i += 1;
        } else if b"<>=!~".contains(&bytes[i]) {
            while i < bytes.len() && b"<>=!~".contains(&bytes[i]) {
                i += 1;
            }
        } else {
            while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                i += 1;
            }
            ensure!(i > start, "unsupported requirement marker token");
        }
        tokens.push(&source[start..i]);
    }
    fn operand(tokens: &[&str], at: &mut usize) -> Result<()> {
        let token = tokens
            .get(*at)
            .context("requirement marker operand is absent")?;
        ensure!(
            token.starts_with(['\'', '"'])
                || matches!(
                    *token,
                    "python_version"
                        | "python_full_version"
                        | "os_name"
                        | "sys_platform"
                        | "platform_release"
                        | "platform_system"
                        | "platform_version"
                        | "platform_machine"
                        | "platform_python_implementation"
                        | "implementation_name"
                        | "implementation_version"
                        | "extra"
                        | "extras"
                        | "dependency_groups"
                ),
            "unsupported requirement marker variable"
        );
        *at += 1;
        Ok(())
    }
    fn expression(tokens: &[&str], at: &mut usize, depth: usize) -> Result<()> {
        ensure!(depth < 64, "requirement marker nesting limit exceeded");
        loop {
            if tokens.get(*at) == Some(&"(") {
                *at += 1;
                expression(tokens, at, depth + 1)?;
                ensure!(
                    tokens.get(*at) == Some(&")"),
                    "unclosed requirement marker group"
                );
                *at += 1;
            } else {
                operand(tokens, at)?;
                let op = *tokens
                    .get(*at)
                    .context("requirement marker comparison is absent")?;
                *at += 1;
                if op == "not" {
                    ensure!(
                        tokens.get(*at) == Some(&"in"),
                        "invalid requirement marker not-in operator"
                    );
                    *at += 1;
                } else {
                    ensure!(
                        matches!(
                            op,
                            "in" | "===" | "~=" | "==" | "!=" | "<=" | ">=" | "<" | ">"
                        ),
                        "invalid requirement marker comparison"
                    );
                }
                operand(tokens, at)?;
            }
            if matches!(tokens.get(*at), Some(&"and" | &"or")) {
                *at += 1;
            } else {
                return Ok(());
            }
        }
    }
    let mut at = 0;
    expression(&tokens, &mut at, 0)?;
    ensure!(at == tokens.len(), "unexpected requirement marker suffix");
    Ok(())
}

fn requirements(source: &str) -> Result<()> {
    let mut logical = String::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        logical.push_str(line.strip_suffix('\\').unwrap_or(line));
        if line.ends_with('\\') {
            logical.push(' ');
            continue;
        }
        let comment = logical.char_indices().find_map(|(at, c)| {
            (c == '#' && logical[..at].ends_with(char::is_whitespace)).then_some(at)
        });
        let requirement = logical[..comment.unwrap_or(logical.len())].trim();
        if requirement.starts_with('-') {
            let fields: Vec<_> = requirement.split_whitespace().collect();
            let option = fields[0].split('=').next().unwrap_or_default();
            let valued = matches!(
                option,
                "-r" | "--requirement"
                    | "-c"
                    | "--constraint"
                    | "-e"
                    | "--editable"
                    | "-i"
                    | "--index-url"
                    | "--extra-index-url"
                    | "-f"
                    | "--find-links"
                    | "--trusted-host"
                    | "--only-binary"
                    | "--no-binary"
                    | "--use-feature"
                    | "--use-deprecated"
            );
            ensure!(
                (valued && (fields.len() == 2 || fields[0].contains('=')))
                    || (matches!(
                        option,
                        "--no-index" | "--pre" | "--prefer-binary" | "--require-hashes"
                    ) && fields.len() == 1),
                "unsupported requirements option syntax"
            );
        } else {
            // These pip options belong to one requirement; they are not
            // package version or marker syntax, and are never executed here.
            let mut requirement_parts = requirement.split(" --");
            let requirement = requirement_parts.next().unwrap_or_default();
            for option in requirement_parts {
                let (name, value) = option
                    .split_once('=')
                    .or_else(|| option.split_once(' '))
                    .context("requirement option requires a value")?;
                ensure!(
                    matches!(
                        name,
                        "hash" | "config-settings" | "global-option" | "install-option"
                    ) && !value.trim().is_empty(),
                    "unsupported per-requirement option syntax"
                );
            }
            // A direct URL may contain semicolons. PEP 508 terminates its
            // URL with whitespace before the marker separator.
            let direct_url = requirement.contains('@') || url::Url::parse(requirement).is_ok();
            let separator = requirement.char_indices().find_map(|(at, c)| {
                (c == ';' && (!direct_url || requirement[..at].ends_with(char::is_whitespace)))
                    .then_some(at)
            });
            let (base, marker) = separator.map_or((requirement, None), |at| {
                (&requirement[..at], Some(&requirement[at + 1..]))
            });
            if let Some(marker) = marker {
                requirement_marker(marker)?;
            }
            let base = base.trim();
            let local = base.trim_matches(['\'', '"']);
            if local.starts_with("./")
                || local.starts_with("../")
                || local.starts_with('/')
                || (local.as_bytes().get(1) == Some(&b':')
                    && local
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphabetic))
                || url::Url::parse(base).is_ok()
            {
                ensure!(
                    !local.contains(['\n', '\r']),
                    "invalid local requirement path"
                );
                logical.clear();
                continue;
            }
            let name_end = base
                .find(|c: char| !c.is_ascii_alphanumeric() && !"._-".contains(c))
                .unwrap_or(base.len());
            ensure!(
                name_end > 0 && base.as_bytes()[0].is_ascii_alphanumeric(),
                "invalid requirement name"
            );
            let mut rest = base[name_end..].trim();
            if let Some(extras) = rest.strip_prefix('[') {
                let end = extras
                    .find(']')
                    .context("unterminated requirement extras")?;
                ensure!(
                    extras[..end].trim().is_empty()
                        || extras[..end].split(',').all(|e| !e.trim().is_empty()
                            && e.trim()
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))),
                    "invalid requirement extras"
                );
                rest = extras[end + 1..].trim();
            }
            if let Some(url) = rest.strip_prefix('@') {
                url::Url::parse(url.trim()).context("unsupported direct requirement URL")?;
            } else if !rest.is_empty() {
                if rest.starts_with('(') && rest.ends_with(')') {
                    rest = &rest[1..rest.len() - 1];
                }
                // The version_many grammar permits one trailing comma.
                let clauses = rest.trim().strip_suffix(',').unwrap_or(rest).trim();
                for clause in clauses.split(',') {
                    let clause = clause.trim();
                    let op = ["===", "~=", "==", "!=", ">=", "<=", ">", "<"]
                        .into_iter()
                        .find(|op| clause.starts_with(op))
                        .context("unsupported requirement version syntax")?;
                    let version = clause[op.len()..].trim();
                    ensure!(
                        (op == "==="
                            || version
                                .strip_prefix(['v', 'V'])
                                .unwrap_or(version)
                                .as_bytes()
                                .first()
                                .is_some_and(u8::is_ascii_digit))
                            && !version.is_empty()
                            && version
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || ".*+!_-".contains(c)),
                        "invalid requirement version"
                    );
                }
            }
        }
        logical.clear();
    }
    ensure!(logical.is_empty(), "unterminated requirement continuation");
    Ok(())
}

fn bracket_end(source: &str, at: usize) -> Result<Option<usize>> {
    let bytes = source.as_bytes();
    if bytes.get(at) != Some(&b'[') {
        return Ok(None);
    }
    let mut i = at + 1;
    while bytes.get(i) == Some(&b'=') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'[') {
        return Ok(None);
    }
    let end = format!("]{}]", "=".repeat(i - at - 1));
    let offset = source[i + 1..]
        .find(&end)
        .context("unterminated CMake bracket argument")?;
    Ok(Some(i + 1 + offset + end.len()))
}
fn cmake(source: &str) -> Result<()> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut blocks: Vec<String> = Vec::new();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == b'#' {
            if let Some(end) = bracket_end(source, i + 1)? {
                i = end;
            } else {
                i = source[i..].find('\n').map_or(bytes.len(), |n| i + n + 1);
            }
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        ensure!(
            i > start && (bytes[start].is_ascii_alphabetic() || bytes[start] == b'_'),
            "unsupported CMake command syntax"
        );
        let command = source[start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        ensure!(
            bytes.get(i) == Some(&b'('),
            "CMake command requires parenthesized arguments"
        );
        i += 1;
        let mut depth = 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'#' => {
                    if let Some(end) = bracket_end(source, i + 1)? {
                        i = end;
                    } else {
                        i = source[i..].find('\n').map_or(bytes.len(), |n| i + n + 1);
                    }
                }
                b'[' => {
                    if let Some(end) = bracket_end(source, i)? {
                        i = end;
                    } else {
                        i += 1;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    ensure!(i < bytes.len(), "unterminated CMake quoted argument");
                    i += 1;
                }
                b'\\' => {
                    ensure!(i + 1 < bytes.len(), "unterminated CMake escape");
                    i += 2;
                }
                b'(' => {
                    depth += 1;
                    i += 1;
                }
                b')' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        ensure!(depth == 0, "unterminated CMake command");
        if matches!(
            command.as_str(),
            "if" | "foreach" | "while" | "function" | "macro" | "block"
        ) {
            blocks.push(command);
        } else if let Some(kind) = command.strip_prefix("end")
            && matches!(
                kind,
                "if" | "foreach" | "while" | "function" | "macro" | "block"
            )
        {
            ensure!(
                blocks.pop().as_deref() == Some(kind),
                "mismatched CMake control block"
            );
        }
    }
    ensure!(blocks.is_empty(), "unterminated CMake control block");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_module_blocks_quotes_and_noncanonical_versions_preserve_extraction() {
        let source = "module (\n \"example.com/\\app\"\n)\nrequire (\n `example.com/dep` master\n example.com/other abc123\n)\nreplace example.com/dep=>\"../local\\ dir\"\n";
        let manifest = go_manifest(source).unwrap();
        assert_eq!(manifest.package_name.as_deref(), Some("example.com/app"));
        assert_eq!(
            manifest.dependencies,
            ["example.com/dep", "example.com/other"]
        );
        assert!(go_manifest("module (\nexample.com/a\nexample.com/b\n)\n").is_err());
        assert!(go_manifest("module \"example.com/app\\\"").is_err());
    }

    #[test]
    fn requirements_preserve_paths_options_urls_and_pep508_markers() {
        for source in [
            "requests[]\nrequests[ ]>=2.0\nrequests>=2.0,\nrequests (>=2.0, )\n",
            "requests==2.32.0\t# pinned\n",
            "demo @ https://example.test/demo.whl?key=a;b\n",
            "demo @ https://example.test/demo.whl?key=a;b ; python_version >= '3.10'\n",
            "requests==v2.0\nrequests~=V2.0\n",
            "./downloads/package.whl\n../project\n/opt/packages/local.tar.gz\n",
            "requests; python_version ~= '3.10'\nrequests; python_full_version === '3.10.0'\n",
            "requests; (python_version >= '3.10' and os_name not in 'nt') or extra == 'web'\n",
            "--use-feature fast-deps\nrequests --config-settings=build-option=value\n",
            "https://example.test/downloads/package.whl\ngit+https://example.test/project.git@main#egg=project\n",
            "requests @ https://example.test/requests.whl ; python_version >= '3.10'\n",
        ] {
            requirements(source).unwrap_or_else(|e| panic!("{source}: {e:#}"));
        }
        for source in [
            "requests>=2.0,,\n",
            "requests[,security]\n",
            "demo @ https://example.test/demo.whl ; python_version >=\n",
            "requests; python_version ~=\n",
            "requests; python_version = '3.10'\n",
            "requests; python_version + '3.10'\n",
            "requests; unknown_variable == 'value'\n",
            "requests --config-settings\n",
        ] {
            assert!(requirements(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn supported_manifest_syntax_accepts_ordinary_valid_inputs() {
        for (path, source) in [
            (
                "package.json",
                r#"{"name":"app","dependencies":{"dep":"1"}}"#,
            ),
            ("Cargo.toml", "[workspace]\nmembers=[]\n"),
            (
                "pyproject.toml",
                "[project]\nname='app'\ndependencies=['requests>=2']\n",
            ),
            ("composer.json", r#"{"require":{"php":"^8.0"}}"#),
            (
                "pubspec.yaml",
                "name: app\ndependencies:\n  flutter:\n    sdk: flutter\n",
            ),
            (
                "go.mod",
                "module example.com/app\ngo 1.23.0\nrequire (\n example.com/dep v1.2.0 // indirect\n)\nreplace example.com/dep => ../dep\nretract [v1.0.0, v1.0.1]\n",
            ),
            (
                "Gemfile",
                "source 'https://rubygems.org'\ngroup :development do\n gem 'rake', '~> 13.0'\nend\n",
            ),
            (
                "Package.swift",
                "// swift-tools-version: 5.9\nimport PackageDescription\nlet package = Package(name: \"App\", targets: [.target(name: \"App\")])\n",
            ),
            (
                "build.gradle.kts",
                "plugins { java }\ndependencies { implementation(\"org.example:lib:1.0\") }\n",
            ),
            (
                "app.csproj",
                "<Project Sdk=\"Microsoft.NET.Sdk\"><ItemGroup><PackageReference Include=\"Example\" Version=\"1.0\"/></ItemGroup></Project>",
            ),
            (
                "requirements.txt",
                "requests[security]>=2.0,<3; python_version >= '3.10'\n-r other.txt\n--index-url https://example.test/simple\nflask==3.0 \\\n --hash=sha256:abc\n",
            ),
            (
                "CMakeLists.txt",
                "\u{feff}cmake_minimum_required(VERSION 3.20)\nproject(App)\n",
            ),
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.20)\nproject(App)\n#[=[ bracket comment ]=]\nif(TRUE)\n add_executable(app main.cpp)\n message([=[a (bracket) argument]=])\nendif()\n",
            ),
        ] {
            validate(Path::new(path), source).unwrap_or_else(|error| panic!("{path}: {error:#}"));
        }
        for path in ["Gemfile", "requirements.txt", "CMakeLists.txt"] {
            validate(Path::new(path), "# no dependencies\n").unwrap();
        }
    }

    #[test]
    fn malformed_manifest_never_becomes_confirmed_empty_input() {
        for (path, source) in [
            ("package.json", r#"{"dependencies":[]}"#),
            ("Cargo.toml", "dependencies=7"),
            ("pyproject.toml", "[project]\ndependencies='requests'"),
            ("composer.json", r#"{"require":false}"#),
            ("pubspec.yaml", "dependencies: false"),
            ("go.mod", "this is not a Go module"),
            (
                "go.mod",
                "module example.com/app\nrequire (\nexample.com/dep v1.0.0",
            ),
            ("go.mod", "module example.com/app\nrequire example.com/dep"),
            ("Gemfile", "gem 'unterminated"),
            ("Package.swift", "let package = Package("),
            ("build.gradle.kts", "dependencies { implementation("),
            ("app.csproj", "<Project><PackageReference></Project>"),
            ("app.csproj", "not XML"),
            ("requirements.txt", "this is not a requirement"),
            ("requirements.txt", "requests >="),
            ("requirements.txt", "requests; python_version >="),
            ("CMakeLists.txt", "this is not a CMake manifest"),
            ("CMakeLists.txt", "project(unterminated"),
            ("CMakeLists.txt", "if(TRUE)\nproject(App)"),
        ] {
            assert!(
                validate(Path::new(path), source).is_err(),
                "accepted {path}: {source}"
            );
        }
    }
}
