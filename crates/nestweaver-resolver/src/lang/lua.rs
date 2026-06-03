use std::collections::HashSet;

/// Resolve a Lua `require("module.name")` to a file path.
///
/// Converts dots to `/` → tries `module/name.lua` and `module/name/init.lua`.
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let path = specifier.replace('.', "/");

    let candidates = [format!("{path}.lua"), format!("{path}/init.lua")];

    let mut best: Option<&str> = None;
    for candidate in &candidates {
        for &file in known_files {
            if file == candidate.as_str() || file.ends_with(&format!("/{candidate}")) {
                if best.is_none() || file.len() < best.unwrap().len() {
                    best = Some(file);
                }
            }
        }
    }
    if let Some(f) = best {
        return Some(f.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn resolves_dotted_module() {
        let known = set(&["src/module/name.lua"]);
        let result = resolve_import("src/main.lua", "module.name", &known);
        assert_eq!(result, Some("src/module/name.lua".to_string()));
    }

    #[test]
    fn resolves_init_lua() {
        let known = set(&["lib/module/name/init.lua"]);
        let result = resolve_import("src/main.lua", "module.name", &known);
        assert_eq!(result, Some("lib/module/name/init.lua".to_string()));
    }

    #[test]
    fn resolves_simple_module() {
        let known = set(&["utils.lua"]);
        let result = resolve_import("main.lua", "utils", &known);
        assert_eq!(result, Some("utils.lua".to_string()));
    }

    #[test]
    fn unknown_module_returns_none() {
        let known = set(&["src/other.lua"]);
        let result = resolve_import("src/main.lua", "missing", &known);
        assert_eq!(result, None);
    }
}
