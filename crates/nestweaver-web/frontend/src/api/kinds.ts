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
