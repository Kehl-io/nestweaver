use crate::entry_points::detect_entry_point;
use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `@interface ClassName` (optionally with superclass / protocol conformance).
static RE_INTERFACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*@interface\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches `@implementation ClassName`.
static RE_IMPLEMENTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*@implementation\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches `@protocol ProtocolName`.
static RE_PROTOCOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*@protocol\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches instance methods `- (ReturnType)methodName` and class methods `+ (ReturnType)methodName`.
static RE_METHOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-+]\s*\([^)]*\)\s*([a-zA-Z_][a-zA-Z0-9_]*)").unwrap());

/// Matches C-style function definitions (return type followed by name and parens).
static RE_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:static\s+)?(?:inline\s+)?(?:extern\s+)?(?:void|int|float|double|char|long|unsigned|BOOL|id|NSString|NS\w+)\s+\*?\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\(",
    )
    .unwrap()
});

/// Matches `#import "header.h"` or `#import <header.h>`.
static RE_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*#import\s+[<"]([^>"]+)[>"]"#).unwrap());

/// Matches `#include "header.h"` or `#include <header.h>`.
static RE_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*#include\s+[<"]([^>"]+)[>"]"#).unwrap());

/// Matches `[receiver methodName` for method call references.
static RE_MSG_SEND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\w+)\s+([a-zA-Z_][a-zA-Z0-9_]*)").unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse an Objective-C source file using regex-based line scanning.
pub fn parse_objc(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();
    let file_path_str = path.to_string_lossy();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();
    let mut in_class = false;

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let trimmed = line.trim();

        // Skip single-line comments
        if trimmed.starts_with("//") {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_INTERFACE.captures(line) {
            let name = cap[1].to_string();
            in_class = true;
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Interface,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_IMPLEMENTATION.captures(line) {
            let name = cap[1].to_string();
            in_class = true;
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Class,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_PROTOCOL.captures(line) {
            let name = cap[1].to_string();
            // Skip `@protocol Foo;` forward declarations
            if trimmed.ends_with(';') && !trimmed.contains('<') {
                continue;
            }
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Interface,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if trimmed == "@end" {
            in_class = false;
            continue;
        }

        if let Some(cap) = RE_METHOD.captures(line) {
            let name = cap[1].to_string();
            let kind = if in_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let ep_kind =
                detect_entry_point(&name, &file_path_str, "method", Some(trimmed), "objc");
            symbols.push(RawSymbol {
                name,
                kind,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: ep_kind.is_some(),
                entry_point_kind: ep_kind,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if !in_class && let Some(cap) = RE_FUNCTION.captures(line) {
            let name = cap[1].to_string();
            let visibility = if trimmed.starts_with("static ") {
                Visibility::Private
            } else {
                Visibility::Public
            };
            let ep_kind =
                detect_entry_point(&name, &file_path_str, "function", Some(trimmed), "objc");
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Function,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: ep_kind.is_some(),
                entry_point_kind: ep_kind,
                visibility,
                type_info: None,
            });
            continue;
        }

        // ── reference detection ────────────────────────────────────────────

        if let Some(cap) = RE_IMPORT.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }

        if let Some(cap) = RE_INCLUDE.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Includes,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }

        for cap in RE_MSG_SEND.captures_iter(line) {
            let method_name = cap[2].to_string();
            // Skip common keywords
            if matches!(
                method_name.as_str(),
                "if" | "for" | "while" | "return" | "self" | "super"
            ) {
                continue;
            }
            references.push(RawReference {
                name: method_name,
                kind: ReferenceKind::Call,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }
    }

    ParsedFile {
        path: path_str,
        symbols,
        references,
    }
}
