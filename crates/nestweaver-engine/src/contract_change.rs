//! Contract-change detection wired to git.
//!
//! Bridges the pure [`crate::signature_diff`] engine to a live PR: for each
//! changed file it reconstructs the BEFORE snapshot from `git show
//! <base>:<file>` and the AFTER snapshot from the working tree on disk, parses
//! both with [`parse_source`], adapts each [`RawSymbol`] into a schema
//! [`Symbol`] carrying only the API-relevant fields, and runs
//! [`diff_public_api`] over the two snapshots.
//!
//! Everything is best-effort: a file that didn't exist at `base` (added file)
//! has an empty BEFORE, a file missing from the working tree (deleted file) has
//! an empty AFTER, and a `git show` / disk read / parse failure on either side
//! is skipped rather than propagated — a single unparsable file must never
//! crash the whole diff.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use nestweaver_parser::{RawSymbol, detect_language, parse_source};
use nestweaver_schema::{Symbol, SymbolKind, TypeInfo};

use crate::signature_diff::{BreakingChange, diff_public_api};

/// Extract the balanced top-level parameter-list substring of a signature: the
/// text between the first `(` and its matching `)`. `None` when the signature
/// has no parameter list (e.g. a constant or a brace-only type declaration).
fn param_list_str(sig: &str) -> Option<&str> {
    let open = sig.find('(')?;
    let mut depth = 0i32;
    for (i, ch) in sig.char_indices().skip(open) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&sig[open + ch.len_utf8()..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a parameter list on top-level commas, ignoring commas nested inside
/// `()[]{}` (tuples, closures, destructuring) or `<>` generics — so
/// `x: Map<K, V>, y: i32` yields two parameters, not three.
///
/// Generic angle brackets are tracked in a *separate* counter that never goes
/// negative and skips `->`/`=>` arrows (whose `>` would otherwise be read as a
/// generic close and corrupt the depth). That keeps closure-typed params like
/// `cb: impl Fn(i32) -> i32, x: i32` splitting correctly.
fn split_top_level_params(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32; // () [] {} — always balanced
    let mut angle = 0i32; // <> generics — tracked apart, never negative
    let mut prev = '\0';
    let mut cur = String::new();
    for ch in list.chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(ch);
            }
            // `<` opens a generic unless it's part of `<<` (shift).
            '<' if prev != '<' => {
                angle += 1;
                cur.push(ch);
            }
            // `>` closes a generic only when one is open and it isn't the `>` of
            // an `->`/`=>` arrow.
            '>' if angle > 0 && prev != '-' && prev != '=' => {
                angle -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 && angle == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
        prev = ch;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Strip binding modifiers (`&`, `mut`, `ref`) from a parameter name token.
fn clean_param_name(s: &str) -> String {
    s.trim()
        .trim_start_matches('&')
        .trim()
        .trim_start_matches("mut ")
        .trim()
        .trim_start_matches("ref ")
        .trim()
        .to_string()
}

/// Parse a single parameter into `(name, Option<type>)`. Handles the two common
/// shapes: colon-typed (`name: T` — Rust/TS/Python/Swift/Kotlin) and
/// whitespace-separated (`T name` / `name T` — Java/C/Go), taking the last token
/// as the name. `None` for an empty fragment.
fn parse_param(raw: &str) -> Option<(String, Option<String>)> {
    let p = raw.trim();
    if p.is_empty() {
        return None;
    }
    if let Some((name, ty)) = p.split_once(':') {
        let name = clean_param_name(name);
        if name.is_empty() {
            return None;
        }
        let ty = ty.trim().trim_end_matches(['{', ',']).trim();
        return Some((name, (!ty.is_empty()).then(|| ty.to_string())));
    }
    let toks: Vec<&str> = p.split_whitespace().collect();
    match toks.as_slice() {
        [] => None,
        [one] => Some((clean_param_name(one), None)),
        _ => {
            let name = clean_param_name(toks[toks.len() - 1]);
            let ty = toks[..toks.len() - 1].join(" ");
            Some((name, (!ty.is_empty()).then_some(ty)))
        }
    }
}

/// Reconstruct `parameter_types` from a function/method signature.
///
/// The parser fills `type_info.return_type` but leaves `parameter_types` empty
/// (parameter typing is a later resolution concern the raw parse skips), so the
/// signature string is the source of truth for arity + per-position types when
/// diffing two snapshots. Only extracted for param-bearing symbol kinds.
fn extract_params_from_signature(sig: &str) -> Vec<(String, Option<String>)> {
    match param_list_str(sig) {
        Some(list) => split_top_level_params(list)
            .iter()
            .filter_map(|p| parse_param(p))
            .collect(),
        None => Vec::new(),
    }
}

/// Adapt a parser [`RawSymbol`] into a schema [`Symbol`] carrying only the
/// fields the signature-diff engine reads (name, kind, signature, visibility,
/// `type_info`, `content_hash`, `file_path`, `start_line`).
///
/// Identity is synthesized so [`diff_public_api`] matches BEFORE↔AFTER by
/// `(name, file_path)`: `canonical_id` is `None`, `repo_uid` is empty, and
/// `uid` is `"<file>:<name>"`. Everything else defaults to its empty/`None`
/// form — these symbols exist only to be diffed, not persisted.
fn raw_to_api_symbol(raw: &RawSymbol, file: &Path) -> Symbol {
    let file_display = file.display().to_string();
    Symbol {
        uid: format!("{}:{}", file_display, raw.name),
        name: raw.name.clone(),
        kind: raw.kind,
        repo_uid: String::new(),
        file_path: file_display,
        start_line: raw.start_line,
        end_line: raw.end_line,
        // Declaration only — the parser's signature includes the body, which
        // would defeat the diff engine's body-only filter (signature equality).
        signature: strip_body(&raw.signature),
        summary: None,
        content_hash: raw.content_hash.clone(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: raw.visibility,
        type_info: enriched_type_info(raw),
        framework_hint: None,
        canonical_id: None,
    }
}

/// Strip a symbol's body from its signature/type text. The parser's `signature`
/// (and, for some languages, `return_type`) includes the function body — e.g.
/// `fn foo(a: i32) -> i32 { a + 1 }` — but the API surface is the *declaration*
/// only.
///
/// The body brace is the first `{` at bracket-depth 0 (outside the parameter
/// list's `(...)`/`[...]`). Cutting there — rather than at the *first* `{` —
/// preserves destructured parameters like TS/JS `function f({ a, b }: Props)`,
/// whose braces sit inside the parens and would otherwise be mistaken for the
/// body, truncating the whole signature to `function f(`. (A TS object *return*
/// type `): { ok: bool }` is still cut at its brace — a pre-existing limitation,
/// unchanged here — but the diff engine reads the return type from `type_info`.)
fn strip_body(s: &str) -> String {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' if depth <= 0 => return s[..i].trim_end().to_string(),
            _ => {}
        }
    }
    s.trim_end().to_string()
}

/// Build the `type_info` the diff engine reads, backfilling `parameter_types`
/// from the signature for param-bearing kinds (the raw parse leaves them empty)
/// and stripping any body captured into `return_type`.
fn enriched_type_info(raw: &RawSymbol) -> Option<TypeInfo> {
    let params = match raw.kind {
        SymbolKind::Function | SymbolKind::Method => extract_params_from_signature(&raw.signature),
        _ => Vec::new(),
    };
    // The parser sometimes captures the body into return_type; keep the
    // declaration only so body edits don't look like a return-type change.
    let sanitize_ret = |ti: &TypeInfo| ti.return_type.as_deref().map(strip_body);

    match (&raw.type_info, params.is_empty()) {
        // No params to backfill, but still sanitize a body-laden return_type.
        (Some(ti), true) => Some(TypeInfo {
            declared_type: ti.declared_type.clone(),
            parameter_types: ti.parameter_types.clone(),
            return_type: sanitize_ret(ti),
        }),
        // `None` type_info stays None so the engine's coarse signature fallback
        // can still fire on a genuine signature change.
        (None, true) => None,
        (Some(ti), false) => Some(TypeInfo {
            declared_type: ti.declared_type.clone(),
            parameter_types: params,
            return_type: sanitize_ret(ti),
        }),
        (None, false) => Some(TypeInfo {
            declared_type: None,
            parameter_types: params,
            return_type: None,
        }),
    }
}

/// Parse `source` for `file` and adapt its symbols into API-diff [`Symbol`]s.
///
/// A [`ParseError`](nestweaver_parser::ParseError) yields an empty vec — a
/// parse failure on one side is treated as "no symbols" (best-effort), never an
/// error for the whole run.
fn parse_api_symbols(file: &Path, source: &str) -> Vec<Symbol> {
    match parse_source(file, source) {
        Ok(parsed) => parsed
            .symbols
            .iter()
            .map(|raw| raw_to_api_symbol(raw, file))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Read a file's content at `base_ref` via `git -C <repo> show <base>:<file>`.
///
/// Returns `None` when the command fails for any reason — most commonly the
/// file did not exist at `base` (an added file), which the caller treats as an
/// empty BEFORE snapshot.
fn git_show(repo_root: &Path, base_ref: &str, file: &Path) -> Option<String> {
    let spec = format!("{}:{}", base_ref, file.display());
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show")
        .arg(&spec)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Compute contract-verified breaking API changes for a set of changed files by
/// diffing each file's BEFORE (git `base_ref`) and AFTER (working tree)
/// signatures.
///
/// Only files [`detect_language`] recognizes are diffed. For each such file:
/// BEFORE = `git show <base_ref>:<file>` (empty if the file was added), AFTER =
/// the working-tree file on disk (empty if it was deleted). Deleted files turn
/// all their public BEFORE symbols into `Removed`; added files produce nothing
/// (additions aren't breaking). The aggregated result is sorted deterministically
/// by `(symbol_uid, kind)`.
pub fn breaking_changes_from_git(
    repo_root: &Path,
    base_ref: &str,
    changed_files: &[PathBuf],
) -> Result<Vec<BreakingChange>> {
    let mut out = Vec::new();

    for file in changed_files {
        // Only diff recognized source files; skip configs, data, docs, etc.
        if detect_language(file).is_none() {
            continue;
        }

        let before_syms = git_show(repo_root, base_ref, file)
            .map(|src| parse_api_symbols(file, &src))
            .unwrap_or_default();

        let after_syms = std::fs::read_to_string(repo_root.join(file))
            .ok()
            .map(|src| parse_api_symbols(file, &src))
            .unwrap_or_default();

        out.extend(diff_public_api(&before_syms, &after_syms));
    }

    // `diff_public_api` already sorts per-file; impose a stable overall order so
    // the aggregate across files is deterministic regardless of input order.
    out.sort_by(|a, b| {
        a.symbol_uid
            .cmp(&b.symbol_uid)
            .then_with(|| format!("{:?}", a.kind).cmp(&format!("{:?}", b.kind)))
    });

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature_diff::{BreakKind, BreakTier};
    use nestweaver_schema::Visibility;

    /// Whether `git` is on PATH; git-backed tests skip gracefully without it.
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run a git subcommand in `dir`, asserting it succeeds.
    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Init an isolated repo (no signing / template surprises).
    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn extract_params_handles_generics_and_colon_types() {
        let sig = "pub fn f(a: i32, b: Map<K, V>, c: &str) -> i32 {";
        let params = extract_params_from_signature(sig);
        assert_eq!(
            params,
            vec![
                ("a".to_string(), Some("i32".to_string())),
                ("b".to_string(), Some("Map<K, V>".to_string())),
                ("c".to_string(), Some("&str".to_string())),
            ]
        );
    }

    #[test]
    fn extract_params_empty_arg_list() {
        assert!(extract_params_from_signature("pub fn f() {").is_empty());
        assert!(extract_params_from_signature("pub const X: i32 = 5;").is_empty());
    }

    /// A closure-typed param contains `->`, whose `>` must NOT be read as a
    /// generic close (which would swallow the following comma and merge params).
    #[test]
    fn extract_params_handles_arrow_in_closure_type() {
        let sig = "pub fn f(cb: impl Fn(i32) -> i32, x: i32) -> i32 {";
        let params = extract_params_from_signature(sig);
        assert_eq!(
            params,
            vec![
                ("cb".to_string(), Some("impl Fn(i32) -> i32".to_string())),
                ("x".to_string(), Some("i32".to_string())),
            ]
        );
    }

    /// `strip_body` must cut at the *body* brace, not a destructured param's
    /// brace — otherwise a TS/JS `function f({ a, b }: Props)` truncates to
    /// `function f(` and every diff against it is bogus.
    #[test]
    fn strip_body_preserves_destructured_params() {
        assert_eq!(
            strip_body("function f({ a, b }: Props): void { return; }"),
            "function f({ a, b }: Props): void"
        );
        // Plain function body still stripped.
        assert_eq!(
            strip_body("fn foo(a: i32) -> i32 { a + 1 }"),
            "fn foo(a: i32) -> i32"
        );
        // No body → unchanged (trimmed).
        assert_eq!(strip_body("fn foo(a: i32) -> i32"), "fn foo(a: i32) -> i32");
    }

    /// The raw→API adapter preserves the diff-relevant fields and synthesizes an
    /// identity that forces `(name, file_path)` matching. Not git-dependent.
    #[test]
    fn raw_to_api_symbol_synthesizes_identity() {
        let src = "pub fn foo(a: i32) -> i32 {\n    a\n}\n";
        let file = Path::new("src/lib.rs");
        let syms = parse_api_symbols(file, src);
        let foo = syms
            .iter()
            .find(|s| s.name == "foo")
            .expect("parsed a `foo` symbol");
        assert_eq!(foo.uid, "src/lib.rs:foo");
        assert_eq!(foo.file_path, "src/lib.rs");
        assert!(foo.repo_uid.is_empty());
        assert!(foo.canonical_id.is_none());
        assert_eq!(foo.visibility, Visibility::Public);
        assert!(foo.type_info.is_some(), "tree-sitter-rust resolves params");
    }

    /// Adding a parameter to a committed public fn (changed only in the working
    /// tree) is a `ParamAdded` `Breaking` change.
    /// nw-063 regression: TS `export function` must be visible to the contract
    /// diff (the parser once marked it Private, silently disabling breaking-
    /// change detection for every TypeScript export).
    #[test]
    fn breaking_changes_ts_param_added() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        init_repo(repo);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/api.ts"),
            "export function chargeCustomer(id: string, amount: number): boolean {\n  return true;\n}\n",
        )
        .unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "v1"]);
        std::fs::write(
            repo.join("src/api.ts"),
            "export function chargeCustomer(id: string, amount: number, currency: string): boolean {\n  return true;\n}\n",
        )
        .unwrap();
        let changes =
            breaking_changes_from_git(repo, "HEAD", &[PathBuf::from("src/api.ts")]).unwrap();
        assert_eq!(changes.len(), 1, "expected one TS change: {changes:?}");
        assert_eq!(changes[0].kind, BreakKind::ParamAdded);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
        assert_eq!(changes[0].symbol_name, "chargeCustomer");
    }

    #[test]
    fn breaking_changes_param_added() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        init_repo(repo);

        std::fs::write(
            repo.join("api.rs"),
            "pub fn foo(a: i32) -> i32 {\n    a\n}\n",
        )
        .unwrap();
        git(repo, &["add", "api.rs"]);
        git(repo, &["commit", "-q", "-m", "first"]);

        // Working-tree change: add a second parameter.
        std::fs::write(
            repo.join("api.rs"),
            "pub fn foo(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();

        let changes = breaking_changes_from_git(repo, "HEAD", &[PathBuf::from("api.rs")]).unwrap();
        assert_eq!(changes.len(), 1, "expected exactly one change: {changes:?}");
        assert_eq!(changes[0].kind, BreakKind::ParamAdded);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
        assert_eq!(changes[0].symbol_name, "foo");
    }

    /// A body-only edit (same signature, different body) is NOT a breaking
    /// change — the R12 body-only filter must hold end-to-end.
    #[test]
    fn breaking_changes_body_only_is_not_breaking() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        init_repo(repo);

        std::fs::write(
            repo.join("api.rs"),
            "pub fn foo(a: i32) -> i32 {\n    a\n}\n",
        )
        .unwrap();
        git(repo, &["add", "api.rs"]);
        git(repo, &["commit", "-q", "-m", "first"]);

        // Same signature, different body only.
        std::fs::write(
            repo.join("api.rs"),
            "pub fn foo(a: i32) -> i32 {\n    a + 1\n}\n",
        )
        .unwrap();

        let changes = breaking_changes_from_git(repo, "HEAD", &[PathBuf::from("api.rs")]).unwrap();
        assert!(
            changes.is_empty(),
            "a body-only change must not be breaking: {changes:?}"
        );
    }

    /// Regression: a SINGLE-LINE function body. The parser captures the body
    /// into the signature/return_type here (unlike a multi-line body), so a
    /// body-only edit would otherwise read as a bogus `return-type-changed`
    /// break. `strip_body` must neutralize it.
    #[test]
    fn breaking_changes_single_line_body_only_is_not_breaking() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        init_repo(repo);

        std::fs::write(repo.join("api.rs"), "pub fn foo(a: i32) -> i32 { a + 1 }\n").unwrap();
        git(repo, &["add", "api.rs"]);
        git(repo, &["commit", "-q", "-m", "first"]);

        std::fs::write(repo.join("api.rs"), "pub fn foo(a: i32) -> i32 { a + 2 }\n").unwrap();

        let changes = breaking_changes_from_git(repo, "HEAD", &[PathBuf::from("api.rs")]).unwrap();
        assert!(
            changes.is_empty(),
            "a single-line body-only change must not be breaking: {changes:?}"
        );
    }

    /// Removing a public symbol from the working tree is a `Removed` break.
    #[test]
    fn breaking_changes_removed_symbol() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        init_repo(repo);

        std::fs::write(
            repo.join("api.rs"),
            "pub fn foo(a: i32) -> i32 {\n    a\n}\n",
        )
        .unwrap();
        git(repo, &["add", "api.rs"]);
        git(repo, &["commit", "-q", "-m", "first"]);

        // Working tree no longer defines `foo`.
        std::fs::write(repo.join("api.rs"), "// foo is gone\n").unwrap();

        let changes = breaking_changes_from_git(repo, "HEAD", &[PathBuf::from("api.rs")]).unwrap();
        assert_eq!(changes.len(), 1, "expected exactly one change: {changes:?}");
        assert_eq!(changes[0].kind, BreakKind::Removed);
        assert_eq!(changes[0].tier, BreakTier::Breaking);
        assert_eq!(changes[0].symbol_name, "foo");
    }

    /// A newly added file (absent at `base`) produces no breaking changes —
    /// additions aren't breaking, and the missing BEFORE is treated as empty.
    #[test]
    fn breaking_changes_added_file_is_not_breaking() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        init_repo(repo);

        // Commit an unrelated baseline so HEAD exists.
        std::fs::write(repo.join("seed.rs"), "pub fn seed() {}\n").unwrap();
        git(repo, &["add", "seed.rs"]);
        git(repo, &["commit", "-q", "-m", "seed"]);

        // A brand-new, uncommitted file that did not exist at HEAD.
        std::fs::write(repo.join("new.rs"), "pub fn bar() {}\n").unwrap();

        let changes = breaking_changes_from_git(repo, "HEAD", &[PathBuf::from("new.rs")]).unwrap();
        assert!(
            changes.is_empty(),
            "an added file must not produce breaking changes: {changes:?}"
        );
    }
}
