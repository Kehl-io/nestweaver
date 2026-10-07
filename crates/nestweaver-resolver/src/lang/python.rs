use std::collections::HashSet;

use crate::util::parent_dir;

/// Resolve a Python import specifier to a file path.
///
/// Handles:
/// - Relative imports: `.helper` → sibling `helper.py`
/// - Multi-dot relative imports: `..helper` → parent package's `helper.py`
/// - Relative package imports: `.utils` → `utils/__init__.py`
/// - Absolute dotted imports: `app.helper` → `app/helper.py`
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    if specifier.starts_with('.') {
        // Relative import — count leading dots
        let dot_count = specifier.chars().take_while(|c| *c == '.').count();
        let module_part = &specifier[dot_count..];

        // Start from the immediate parent directory of from_file, then traverse
        // up one additional level for each dot beyond the first.
        // First dot = current package (parent dir of from_file)
        // Each additional dot = go up one more level
        let mut base = parent_dir(from_file).to_string();
        for _ in 1..dot_count {
            base = parent_dir(&base).to_string();
        }

        if module_part.is_empty() {
            // `from . import something` — refers to the package itself
            let candidate = if base.is_empty() {
                "__init__.py".to_string()
            } else {
                format!("{base}/__init__.py")
            };
            if known_files.contains(&candidate.as_str()) {
                return Some(candidate);
            }
            return None;
        }

        let module_path = module_part.replace('.', "/");

        if !base.is_empty() {
            // Try sibling file: base/module_path.py
            let candidate = format!("{base}/{module_path}.py");
            if known_files.contains(&candidate.as_str()) {
                return Some(candidate);
            }

            // Try as package: base/module_path/__init__.py
            let candidate = format!("{base}/{module_path}/__init__.py");
            if known_files.contains(&candidate.as_str()) {
                return Some(candidate);
            }
        } else {
            let candidate = format!("{module_path}.py");
            if known_files.contains(&candidate.as_str()) {
                return Some(candidate);
            }

            let candidate = format!("{module_path}/__init__.py");
            if known_files.contains(&candidate.as_str()) {
                return Some(candidate);
            }
        }
    } else {
        // Only repository-root and the conventional src-layout root are known.
        // Multiple matches are ambiguous; never select an arbitrary local package.
        let module_path = specifier.replace('.', "/");
        let candidates = [
            format!("{module_path}.py"),
            format!("{module_path}/__init__.py"),
            format!("src/{module_path}.py"),
            format!("src/{module_path}/__init__.py"),
        ];
        let mut matches = candidates
            .into_iter()
            .filter(|candidate| known_files.contains(candidate.as_str()));
        let first = matches.next()?;
        return matches.next().is_none().then_some(first);
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
    fn python_resolves_relative_import() {
        let known = set(&["app/helper.py", "app/main.py"]);
        let result = resolve_import("app/main.py", ".helper", &known);
        assert_eq!(result, Some("app/helper.py".to_string()));
    }

    #[test]
    fn python_resolves_init_package() {
        let known = set(&["app/utils/__init__.py", "app/main.py"]);
        let result = resolve_import("app/main.py", ".utils", &known);
        assert_eq!(result, Some("app/utils/__init__.py".to_string()));
    }

    #[test]
    fn python_resolves_absolute_import() {
        let known = set(&["app/models.py"]);
        let result = resolve_import("app/views.py", "app.models", &known);
        assert_eq!(result, Some("app/models.py".to_string()));
    }

    #[test]
    fn python_resolves_absolute_package_import() {
        let known = set(&["app/utils/__init__.py"]);
        let result = resolve_import("app/views.py", "app.utils", &known);
        assert_eq!(result, Some("app/utils/__init__.py".to_string()));
    }

    #[test]
    fn python_unknown_import_returns_none() {
        let known = set(&["app/models.py"]);
        let result = resolve_import("app/views.py", ".missing", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn python_resolves_two_dot_relative_import() {
        // `from ..helper import something` in app/sub/module.py
        // Two dots → go up two levels from app/sub/module.py:
        //   first dot = app/sub (parent dir of module.py)
        //   second dot = app (parent of app/sub)
        // so resolves to app/helper.py
        let known = set(&["app/helper.py", "app/sub/module.py"]);
        let result = resolve_import("app/sub/module.py", "..helper", &known);
        assert_eq!(result, Some("app/helper.py".to_string()));
    }
    #[test]
    fn python_src_layout_is_bounded_and_rejects_root_collisions() {
        let known = set(&[
            "src/qrec/coredata.py",
            "src/qrec/util/__init__.py",
            "tests/test_coredata.py",
        ]);
        assert_eq!(
            resolve_import("tests/test_coredata.py", "qrec.coredata", &known),
            Some("src/qrec/coredata.py".into())
        );
        assert_eq!(
            resolve_import("tests/test_coredata.py", "qrec.util", &known),
            Some("src/qrec/util/__init__.py".into())
        );
        assert_eq!(
            resolve_import("src/qrec/coredata.py", ".util", &known),
            Some("src/qrec/util/__init__.py".into())
        );
        let collision = set(&["qrec/coredata.py", "src/qrec/coredata.py"]);
        assert_eq!(
            resolve_import("tests/test_coredata.py", "qrec.coredata", &collision),
            None
        );
        let unrelated = set(&["vendor/qrec/coredata.py"]);
        assert_eq!(
            resolve_import("tests/test_coredata.py", "qrec.coredata", &unrelated),
            None
        );
    }
}
