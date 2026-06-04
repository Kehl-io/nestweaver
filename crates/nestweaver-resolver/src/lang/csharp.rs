use std::collections::HashSet;

pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let _ = from_file;

    let candidate = format!("{}.cs", specifier.replace('.', "/"));

    let mut best: Option<&str> = None;
    for &file in known_files {
        if (file == candidate || file.ends_with(&format!("/{candidate}")))
            && (best.is_none() || file.len() < best.unwrap().len())
        {
            best = Some(file);
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
    fn resolves_namespace_import() {
        let known = set(&["src/MyApp/Models/User.cs"]);
        let result = resolve_import("src/MyApp/Program.cs", "MyApp.Models.User", &known);
        assert_eq!(result, Some("src/MyApp/Models/User.cs".to_string()));
    }

    #[test]
    fn unknown_namespace_returns_none() {
        let known = set(&["src/MyApp/Models/User.cs"]);
        let result = resolve_import("src/Program.cs", "System.Collections.Generic", &known);
        assert_eq!(result, None);
    }
}
