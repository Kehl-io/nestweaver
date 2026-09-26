use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd},
};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;

/// Largest file `/source` will read. The UI shows a window of lines, so a
/// multi-megabyte file is almost certainly generated or binary; refusing it
/// keeps one request from pulling an arbitrarily large file into memory.
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
pub struct SourceParams {
    pub file: Option<String>,
    /// nw-683: the repo uid whose copy of `file` to serve. Required when
    /// more than one indexed repo has the path.
    pub repo: Option<String>,
    pub line: Option<usize>,
    pub context: Option<usize>,
}

/// Every non-2xx body from this route is `{ error: <machine code>,
/// message: <human text>, file? }`.
fn reject(status: StatusCode, error: &str, message: &str, file: Option<&str>) -> Response {
    let mut body = json!({ "error": error, "message": message });
    if let Some(file) = file {
        body["file"] = json!(file);
    }
    (status, Json(body)).into_response()
}

fn not_found(error: &str, message: &str, file: &str) -> Response {
    reject(StatusCode::NOT_FOUND, error, message, Some(file))
}

fn internal(err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "source: internal error");
    reject(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        &err.to_string(),
        None,
    )
}

const NOT_INDEXED: &str = "this path is not an indexed file of the repo";

/// Outcome of the blocking filesystem half of the request.
enum Read {
    Content(String),
    NotAvailable,
    Escapes,
    Redirected,
    TooLarge,
}

fn read_indexed_file(repo_root: &Path, file: &str) -> Read {
    read_indexed_file_with_hook(repo_root, file, || {})
}

// The hook makes the gap between validation and opening deterministic in the
// race test. Production always passes a no-op.
fn read_indexed_file_with_hook(repo_root: &Path, file: &str, before_open: impl FnOnce()) -> Read {
    let (Ok(canon_root), Ok(canon_path)) = (
        std::fs::canonicalize(repo_root),
        std::fs::canonicalize(repo_root.join(file)),
    ) else {
        return Read::NotAvailable;
    };
    // A symlink inside the repo must not lead outside it.
    if !canon_path.starts_with(&canon_root) {
        return Read::Escapes;
    }
    // nw-682: the resolved file must BE the indexed path. An indexed
    // `src/config.ts` that symlinks to `../.env` resolves inside the root but
    // names an unindexed file, so it fails closed. A case-only mismatch on a
    // case-insensitive volume (APFS) also fails closed here; that is
    // acceptable because indexed paths come from the same on-disk walk and
    // so already carry the on-disk case.
    if canon_path.strip_prefix(&canon_root) != Ok(Path::new(file)) {
        return Read::Redirected;
    }
    // Bounded read: never pull more than the cap (plus one byte to detect
    // overflow) into memory, whatever the metadata said or however the file
    // grows between checks.
    before_open();
    // Open each component relative to a pinned directory descriptor. A path
    // swapped for a symlink after canonicalization must never be followed.
    #[cfg(unix)]
    let opened = open_without_symlinks(&canon_path);
    #[cfg(not(unix))]
    let opened: std::io::Result<std::fs::File> = Err(std::io::ErrorKind::Unsupported.into());
    let Ok(f) = opened else {
        return Read::NotAvailable;
    };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Read::NotAvailable;
    }
    // Read raw bytes and check the size BEFORE decoding: the cap can cut a
    // multi-byte character, and an oversized file must report TooLarge, not
    // the UTF-8 error that cut produces.
    let mut bytes = Vec::new();
    match f.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes) {
        Ok(n) if n as u64 > MAX_SOURCE_BYTES => Read::TooLarge,
        Ok(_) => match String::from_utf8(bytes) {
            Ok(content) => Read::Content(content),
            Err(_) => Read::NotAvailable,
        },
        Err(_) => Read::NotAvailable,
    }
}

#[cfg(unix)]
fn open_without_symlinks(path: &Path) -> std::io::Result<std::fs::File> {
    let root = CString::new("/").expect("constant path");
    // SAFETY: the C string is NUL terminated; a successful fd is owned by File.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: open returned a new owned fd.
    let mut dir = unsafe { std::fs::File::from_raw_fd(fd) };
    let components: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    for (i, part) in components.iter().enumerate() {
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(part.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: dir is open, name is NUL terminated, and the returned fd is
        // transferred into File before dir is dropped.
        let next = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if next < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned fd.
        dir = unsafe { std::fs::File::from_raw_fd(next) };
    }
    Ok(dir)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn source_path_swap_cannot_read_an_unindexed_file() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/config.ts"), "indexed\n").unwrap();
        std::fs::write(temp.path().join("secret"), "secret\n").unwrap();
        let read = read_indexed_file_with_hook(&repo, "src/config.ts", || {
            std::fs::remove_file(repo.join("src/config.ts")).unwrap();
            std::os::unix::fs::symlink(temp.path().join("secret"), repo.join("src/config.ts"))
                .unwrap();
        });
        assert!(matches!(read, Read::NotAvailable));
    }

    #[test]
    fn source_directory_swap_cannot_read_an_unindexed_file() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(repo.join("src/config.ts"), "indexed\n").unwrap();
        std::fs::write(outside.join("config.ts"), "secret\n").unwrap();
        let read = read_indexed_file_with_hook(&repo, "src/config.ts", || {
            std::fs::rename(repo.join("src"), repo.join("old-src")).unwrap();
            std::os::unix::fs::symlink(&outside, repo.join("src")).unwrap();
        });
        assert!(matches!(read, Read::NotAvailable));
    }
}

pub async fn source(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SourceParams>,
) -> Response {
    let Some(file) = params.file.filter(|f| !f.is_empty()) else {
        return reject(
            StatusCode::BAD_REQUEST,
            "file_required",
            "query parameter 'file' is required",
            None,
        );
    };
    // Reject path traversal before anything touches the store or disk. The
    // parent-dir check is per path component, so `src/foo..bar.ts` is fine.
    // nw-627 (5): a NUL can never name a real file and must not reach the OS.
    if Path::new(&file)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
        || file.starts_with('/')
        || file.starts_with('\\')
        || file.contains('\0')
    {
        return reject(
            StatusCode::BAD_REQUEST,
            "invalid_file_path",
            "file must be a repo-relative path without '..' or NUL",
            Some(&file),
        );
    }
    let line = params.line.unwrap_or(1).max(1);
    let context = params.context.unwrap_or(10);

    // nw-682: serve only what the graph indexed. A path with no File node
    // (.env, .git/config, anything gitignored) is never read from disk.
    let indexing = match state.store.repos_indexing_file(&file) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let repo_uid = match params.repo.as_deref().filter(|r| !r.is_empty()) {
        Some(repo) => repo.to_string(),
        None => match indexing.as_slice() {
            [] => return not_found("source_not_indexed", NOT_INDEXED, &file),
            [only] => only.clone(),
            many => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "ambiguous_file",
                        "file": file,
                        "candidates": many,
                        "message": "more than one indexed repo has this path; pass repo=<uid>",
                    })),
                )
                    .into_response();
            }
        },
    };
    // The single Repo lookup for this request.
    let repo = match state.store.lookup_repo(&repo_uid) {
        Ok(Some(repo)) => repo,
        Ok(None) => return not_found("repo_not_found", "no indexed repo has this uid", &file),
        Err(e) => return internal(e),
    };
    if !indexing.contains(&repo_uid) {
        return not_found("source_not_indexed", NOT_INDEXED, &file);
    }
    // Only repos with a known local working tree can serve source from disk.
    let Some(repo_root) = repo.local_root().map(PathBuf::from) else {
        return not_found(
            "source_not_available",
            "the repo has no local working tree",
            &file,
        );
    };

    let read = {
        let file = file.clone();
        tokio::task::spawn_blocking(move || read_indexed_file(&repo_root, &file)).await
    };
    let content = match read {
        Err(join) => return internal(join),
        Ok(Read::Content(c)) => c,
        Ok(Read::NotAvailable) => {
            return not_found(
                "source_not_available",
                "the file is indexed but could not be read from disk",
                &file,
            );
        }
        Ok(Read::Escapes) => {
            return reject(
                StatusCode::BAD_REQUEST,
                "path_escapes_repo",
                "file path escapes repository root",
                Some(&file),
            );
        }
        Ok(Read::Redirected) => return not_found("source_not_indexed", NOT_INDEXED, &file),
        Ok(Read::TooLarge) => {
            return not_found(
                "source_too_large",
                &format!("file is larger than the {MAX_SOURCE_BYTES}-byte preview limit"),
                &file,
            );
        }
    };

    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();
    // An empty file has no lines to window: report start_line 0, end_line 0
    // and no lines, so start never exceeds end.
    if total_lines == 0 {
        return Json(json!({
            "file": file,
            "repo": repo_uid,
            "start_line": 0,
            "end_line": 0,
            "lines": [],
            "total_lines": 0,
        }))
        .into_response();
    }
    // Past-EOF `line` clamps to the last line so the window is never empty
    // and start never exceeds end. All arithmetic saturates: a raw
    // `context + 1` overflow would panic a debug build.
    let line = line.min(total_lines);
    let end = line.saturating_add(context).min(total_lines);
    let start = line.saturating_sub(context.saturating_add(1)).min(end);
    Json(json!({
        "file": file,
        "repo": repo_uid,
        "start_line": start + 1,
        "end_line": end,
        "lines": all_lines[start..end].to_vec(),
        "total_lines": total_lines,
    }))
    .into_response()
}
