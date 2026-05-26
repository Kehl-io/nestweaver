/// Returns the parent directory of a file path (everything before the last `/`).
/// Returns an empty string if there is no `/`.
pub fn parent_dir(path: &str) -> &str {
    path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_returns_directory() {
        assert_eq!(parent_dir("src/main.rs"), "src");
        assert_eq!(parent_dir("a/b/c.rs"), "a/b");
    }

    #[test]
    fn parent_dir_no_slash_returns_empty() {
        assert_eq!(parent_dir("main.rs"), "");
        assert_eq!(parent_dir(""), "");
    }
}
