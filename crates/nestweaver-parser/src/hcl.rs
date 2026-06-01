use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `resource "type" "name" {`.
static RE_RESOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*resource\s+"([^"]+)"\s+"([^"]+)""#).unwrap());

/// Matches `data "type" "name" {`.
static RE_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*data\s+"([^"]+)"\s+"([^"]+)""#).unwrap());

/// Matches `variable "name" {`.
static RE_VARIABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*variable\s+"([^"]+)""#).unwrap());

/// Matches `output "name" {`.
static RE_OUTPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*output\s+"([^"]+)""#).unwrap());

/// Matches `module "name" {`.
static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*module\s+"([^"]+)""#).unwrap());

/// Matches resource references like `aws_instance.web.id` or `var.name`.
static RE_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b((?:var|local|module|data)\.[a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)").unwrap()
});

/// Matches `source = "..."` in module blocks.
static RE_MODULE_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*source\s*=\s*"([^"]+)""#).unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse an HCL/Terraform source file using regex-based line scanning.
///
/// Extracts:
/// - `resource` blocks → [`SymbolKind::Class`] (named as "type.name")
/// - `data` blocks → [`SymbolKind::Class`]
/// - `variable` blocks → [`SymbolKind::Function`]
/// - `output` blocks → [`SymbolKind::Function`]
/// - `module` blocks → [`SymbolKind::Module`]
/// - `var.*`, `module.*`, `data.*` refs → [`ReferenceKind::Call`]
/// - `source = "..."` in modules → [`ReferenceKind::Import`]
pub fn parse_hcl(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;

        // Skip comment lines
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_RESOURCE.captures(line) {
            let type_name = cap[1].to_string();
            let resource_name = cap[2].to_string();
            symbols.push(RawSymbol {
                name: format!("{type_name}.{resource_name}"),
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

        if let Some(cap) = RE_DATA.captures(line) {
            let type_name = cap[1].to_string();
            let data_name = cap[2].to_string();
            symbols.push(RawSymbol {
                name: format!("{type_name}.{data_name}"),
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

        if let Some(cap) = RE_MODULE.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Module,
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

        if let Some(cap) = RE_VARIABLE.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Function,
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

        if let Some(cap) = RE_OUTPUT.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Function,
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

        // ── reference detection ────────────────────────────────────────────

        if let Some(cap) = RE_MODULE_SOURCE.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
                receiver: None,
            });
        }

        for cap in RE_REF.captures_iter(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Call,
                start_line: line_no,
                context: trimmed.to_string(),
                receiver: None,
            });
        }
    }

    ParsedFile {
        path: path_str,
        symbols,
        references,
    }
}
