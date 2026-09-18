export const SYMBOL_KINDS = new Set([
  "symbol",
  "Function",
  "Class",
  "Method",
  "Interface",
  "Trait",
  "Enum",
  "Module",
  "Extension",
  "Constant",
  "Property",
  "TypeAlias",
  "Variable",
]);

export function isSymbolKind(kind?: string | null): boolean {
  return kind != null && SYMBOL_KINDS.has(kind);
}

export function isNoteSelection(
  uid?: string | null,
  kind?: string | null,
): boolean {
  return Boolean(uid?.startsWith("note:") || kind === "note" || kind === "Note");
}

export function isFileSelection(
  uid?: string | null,
  kind?: string | null,
): boolean {
  if (!uid || isNoteSelection(uid, kind)) return false;
  if (
    uid.startsWith("sym:") ||
    uid.startsWith("repo:") ||
    uid.startsWith("svc:") ||
    uid.startsWith("tag:")
  ) {
    return false;
  }
  if (kind?.toLowerCase() === "file") return true;
  return uid.includes("/") || /\.[A-Za-z0-9]+$/.test(uid);
}
