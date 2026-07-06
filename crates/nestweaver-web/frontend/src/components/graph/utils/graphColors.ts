type KindColorKey =
  | "Function"
  | "Method"
  | "Interface"
  | "Class"
  | "Struct"
  | "Trait"
  | "Enum"
  | "Module"
  | "Constant"
  | "Extension"
  | "Note"
  | "Section"
  | "Tag"
  | "File";

export const LIGHT_KIND_COLORS: Record<KindColorKey, string> = {
  Function: "#0e86c4",
  Method: "#2f5fd0",
  Interface: "#0e8f89",
  Class: "#16a355",
  Struct: "#16a355",
  Trait: "#6f9e12",
  Enum: "#c07d0a",
  Module: "#d15f16",
  Constant: "#d13444",
  Extension: "#d15f16",
  Note: "#7a5fc0",
  Section: "#4f6bb0",
  Tag: "#4f8f6c",
  File: "#5a6478",
};

export const DARK_KIND_COLORS: Record<KindColorKey, string> = {
  Function: "#5ed0fe",
  Method: "#5b8def",
  Interface: "#17c7c0",
  Class: "#35d67a",
  Struct: "#35d67a",
  Trait: "#bfe93d",
  Enum: "#ffc13c",
  Module: "#ff8a3c",
  Constant: "#ff5e6c",
  Extension: "#ff8a3c",
  Note: "#b9a6e8",
  Section: "#8fa6d8",
  Tag: "#93c2a8",
  File: "#9aa0ad",
};

export const KIND_COLORS: Record<string, string> = LIGHT_KIND_COLORS;

const EDGE_TYPES = [
  "overview",
  "calls",
  "imports",
  "extends",
  "implements",
  "wikilink",
  "references_code",
  "tagged_with",
  "cross_repo_declared",
  "cross_repo_suggested",
] as const;

export const LIGHT_EDGE_COLORS: Record<string, string> = Object.fromEntries(
  EDGE_TYPES.map((type) => [type, "#6b7280"]),
);

export const DARK_EDGE_COLORS: Record<string, string> = Object.fromEntries(
  EDGE_TYPES.map((type) => [type, "#5d6675"]),
);

export const EDGE_COLORS: Record<string, string> = LIGHT_EDGE_COLORS;

const KIND_ALIASES: Record<string, KindColorKey> = {
  fn: "Function",
  function: "Function",
  method: "Method",
  interface: "Interface",
  ifc: "Interface",
  class: "Class",
  cls: "Class",
  struct: "Struct",
  trait: "Trait",
  enum: "Enum",
  module: "Module",
  mod: "Module",
  constant: "Constant",
  const: "Constant",
  extension: "Extension",
  ext: "Extension",
  note: "Note",
  section: "Section",
  tag: "Tag",
  file: "File",
};

function normalizeKind(kind: string): KindColorKey | null {
  const lastSegment = kind.split("/").filter(Boolean).pop() ?? kind;
  const token = lastSegment.trim().match(/[A-Za-z]+/)?.[0]?.toLowerCase();
  return token ? (KIND_ALIASES[token] ?? null) : null;
}

export function kindColor(kind: string, isDark: boolean): string {
  const normalized = normalizeKind(kind);
  const palette = isDark ? DARK_KIND_COLORS : LIGHT_KIND_COLORS;
  return normalized ? palette[normalized] : (isDark ? "#5d6675" : "#6b7280");
}

export function kindToColor(kind: string): string {
  const isDark = document.documentElement.classList.contains("dark");
  return kindColor(kind, isDark);
}

export function desaturate(hex: string, amount: number): string {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  const gray = Math.round(0.299 * r + 0.587 * g + 0.114 * b);
  const nr = Math.round(r + (gray - r) * amount);
  const ng = Math.round(g + (gray - g) * amount);
  const nb = Math.round(b + (gray - b) * amount);
  return `#${nr.toString(16).padStart(2, "0")}${ng.toString(16).padStart(2, "0")}${nb.toString(16).padStart(2, "0")}`;
}

export function pprToSize(pagerank: number): number {
  const MIN = 6;
  const MAX = 40;
  const logScale = Math.log1p(pagerank * 100);
  return Math.min(MAX, Math.max(MIN, MIN + logScale * 6));
}

export function nodeSize(degree: number, pagerank: number): number {
  return Math.max(6, Math.min(24,
    degree * 1.2 + Math.log(pagerank * 10000 + 1) * 3
  ));
}

export function relevanceToSaturation(
  relevance: number,
  maxRelevance: number,
): number {
  if (maxRelevance <= 0) return 0;
  const normalized = relevance / maxRelevance;
  return 1 - normalized;
}
