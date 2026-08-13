//! Emits `RESPONSE_SHAPE_VERSION`: a content digest of the Rust sources that
//! determine MCP tool response *shapes*.
//!
//! # Why this is derived and not hand-maintained
//!
//! The F16 response cache (`nestweaver_store::cache`) validates an entry on
//! `graph_generation` + `scope_digest` + a 24h TTL. All three describe the
//! GRAPH; none of them describes the BINARY. So a release that adds a field to
//! a cached tool's response (e.g. `brain_search` gaining `semantic_applied` /
//! `degraded_components`) keeps hitting entries written by the PREVIOUS binary
//! for up to 24h and serves the OLD shape — silently reintroducing exactly the
//! ambiguity the new field exists to remove.
//!
//! A constant that a human must remember to bump would fix that release and
//! fail the next one. This digest is derived from the sources themselves, so
//! any change to a shape-relevant crate's `src/` changes it and the cache
//! invalidates *by construction*.
//!
//! It is deliberately over-broad within that set: a comment-only edit also
//! invalidates the cache. That is the correct direction to be wrong in —
//! over-invalidation costs one recompute, under-invalidation costs
//! correctness.
//!
//! # Which crates are hashed
//!
//! Exactly the crates whose code can shape a cached response: this crate's
//! transitive `nestweaver-*` dependency closure, plus `nestweaver-daemon`
//! (its gRPC `brain_search` handler output is forwarded verbatim by
//! `daemon_brain_search_response_to_json`). The closure is *computed* by
//! reading the manifests, not listed here, so a new internal dependency is
//! covered automatically.
//!
//! Crates outside that set (`-web`, `-wasm`, `-client`) are deliberately
//! excluded. They cannot change a cached response shape, and including them
//! would make a one-line edit in any of them recompile this crate's 11k-line
//! `tools.rs` — and then recompile them again, since they depend on it.
//!
//! # Follow-up worth considering
//!
//! A source digest is blunt: it invalidates on edits that cannot possibly
//! change a response shape. The precise alternative is a hand-maintained
//! shape constant guarded by a golden test over each cacheable tool's response
//! key set — exact-scope invalidation, no build-script coupling, and a failing
//! test rather than a memory as the thing that stops you forgetting. That is a
//! better long-term design and a deliberate non-goal here: the mechanism below
//! is correct today, and correct-and-blunt beats clever-and-forgettable.

use std::collections::{BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Crates outside this crate's dependency closure that can still shape a
/// cached response. These are hashed as LEAVES — their own dependencies are
/// not followed. `nestweaver-daemon` qualifies because its gRPC `brain_search`
/// handler output is forwarded verbatim by
/// `daemon_brain_search_response_to_json`; the daemon's own dependency subtree
/// does not, and following it would drag in `nestweaver-web` (a daemon
/// dependency that cannot shape a cached MCP response) purely by association.
/// Everything under the daemon that DOES matter — `-engine`, `-store`,
/// `-proto`, `-federation` — is already reached through this crate's closure.
const EXTRA_SHAPE_CRATE_LEAVES: &[&str] = &["nestweaver-daemon"];

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );
    let out_dir =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is always set by cargo"));

    // Layout detection: require an ancestor `Cargo.toml` that actually declares
    // a `[workspace]`. Testing "the parent has sibling directories containing
    // `src/`" would be satisfied by a registry or vendor directory
    // (`~/.cargo/registry/src/…/nestweaver-mcp-4.1.2` sits beside every other
    // downloaded crate), which would hash unrelated third-party sources and
    // produce a digest that drifts as unrelated crates are fetched. No such
    // build exists today — releases ship binaries, not `cargo publish` — but
    // the check costs nothing and removes the trap.
    let roots = workspace_shape_roots(&manifest_dir);
    let workspace_scoped = !roots.is_empty();
    // Fallback for a standalone/vendored build of just this crate: hash our own
    // `src/`. This is a WEAKER guarantee — a shape change made only in a
    // sibling crate would not move the digest — so it is recorded in the
    // generated file rather than passing silently.
    let roots = if workspace_scoped {
        roots
    } else {
        vec![manifest_dir.join("src")]
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for root in &roots {
        println!("cargo:rerun-if-changed={}", root.display());
        collect_rust_sources(root, &mut files);
    }
    // Sort so the digest does not depend on directory iteration order.
    files.sort();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    "nestweaver-response-shape-v1".hash(&mut hasher);
    std::env::var("CARGO_PKG_VERSION")
        .unwrap_or_default()
        .hash(&mut hasher);
    let mut hashed = 0usize;
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        // Hash the path relative to its root so an absolute checkout location
        // (which does not change behavior) does not change the digest.
        let relative = roots
            .iter()
            .find_map(|root| file.strip_prefix(root).ok().map(|rest| (root, rest)))
            .map(|(root, rest)| {
                let crate_name = root
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{crate_name}/{}", rest.display())
            })
            .unwrap_or_else(|| file.display().to_string());
        relative.replace('\\', "/").hash(&mut hasher);
        bytes.hash(&mut hasher);
        hashed += 1;
    }
    // A digest over zero files would be a constant, i.e. no protection at all.
    // Fail the build rather than ship a cache that cannot tell binaries apart.
    assert!(
        hashed > 0,
        "response-shape digest hashed no sources under {roots:?}; \
         the response cache would not be able to distinguish binaries"
    );
    // `nestweaver_store::cache` treats 0 as "no shape claim" and relies on a
    // real shape version never being 0. Enforce that invariant here instead of
    // merely asserting it in prose: setting the low bit costs one bit of the
    // 64-bit space and makes 0 unreachable by construction.
    let digest = hasher.finish() | 1;

    let generated = format!(
        "// @generated by build.rs — do not edit.\n\
         /// Digest of the sources that determine MCP tool response shapes.\n\
         /// Sources hashed: {hashed} file(s) across {} crate(s); scope: {}.\n\
         pub const RESPONSE_SHAPE_VERSION: u64 = {digest}u64;\n",
        roots.len(),
        if workspace_scoped {
            "shape-relevant workspace crates"
        } else {
            "this crate only (weaker guarantee)"
        }
    );
    std::fs::write(out_dir.join("response_shape_version.rs"), generated)
        .expect("write response_shape_version.rs");

    println!("cargo:rerun-if-changed=build.rs");
}

/// The `src/` directories of every shape-relevant workspace crate, or an empty
/// vec when this is not a workspace build.
fn workspace_shape_roots(manifest_dir: &Path) -> Vec<PathBuf> {
    if find_workspace_root(manifest_dir).is_none() {
        return Vec::new();
    }
    let Some(crates_dir) = manifest_dir.parent() else {
        return Vec::new();
    };
    let Some(self_name) = manifest_dir.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };

    // Breadth-first over internal manifests: start at this crate and follow
    // every `nestweaver-*` dependency. The extra crates are queued as leaves
    // (`traverse = false`) so they contribute their own sources without
    // dragging in their unrelated dependency subtrees.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<(String, bool)> = VecDeque::new();
    queue.push_back((self_name.to_string(), true));
    for extra in EXTRA_SHAPE_CRATE_LEAVES {
        queue.push_back(((*extra).to_string(), false));
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    while let Some((name, traverse)) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let crate_dir = crates_dir.join(&name);
        let manifest = crate_dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        // Track the manifest too: the dependency closure itself is an input.
        println!("cargo:rerun-if-changed={}", manifest.display());
        let src = crate_dir.join("src");
        if src.is_dir() {
            roots.push(src);
        }
        if !traverse {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        for dep in internal_dependency_names(&text) {
            queue.push_back((dep, true));
        }
    }
    roots.sort();
    roots
}

/// Dependency-table keys naming an internal crate. Manifest keys sit at the
/// start of a line, which is what distinguishes them from the same name
/// appearing inside a string or comment.
fn internal_dependency_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in manifest.lines() {
        let Some(rest) = line.strip_prefix("nestweaver-") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        if name.is_empty() {
            continue;
        }
        // Only a dependency-table key is followed by `=`.
        let key = format!("nestweaver-{name}");
        if line[key.len()..].trim_start().starts_with('=') {
            names.push(key);
        }
    }
    names
}

/// Walk up from `start` looking for a `Cargo.toml` that declares `[workspace]`.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let manifest = current.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && text
                .lines()
                .any(|line| line.trim_start().starts_with("[workspace]"))
        {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

fn collect_rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}
