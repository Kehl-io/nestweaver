use crate::util::parent_dir;

pub fn resolve_import(from_file: &str, specifier: &str, known_files: &[&str]) -> Option<String> {
    let specifier = specifier.trim_matches(|c| c == '"' || c == '\'' || c == '<' || c == '>');

    let base_dir = parent_dir(from_file);
    let candidate = if base_dir.is_empty() {
        specifier.to_string()
    } else {
        format!("{base_dir}/{specifier}")
    };

    if known_files.contains(&candidate.as_str()) {
        return Some(candidate);
    }

    if known_files.contains(&specifier) {
        return Some(specifier.to_string());
    }

    let suffix = format!("/{specifier}");
    for &file in known_files {
        if file.ends_with(&suffix) {
            return Some(file.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_include() {
        let known = ["src/sensor.h", "src/main.c"];
        let result = resolve_import("src/main.c", "\"sensor.h\"", &known);
        assert_eq!(result, Some("src/sensor.h".to_string()));
    }

    #[test]
    fn skips_system_include() {
        let known = ["src/main.c"];
        let result = resolve_import("src/main.c", "<stdio.h>", &known);
        assert_eq!(result, None);
    }
}
