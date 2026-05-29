export const KIND_COLORS: Record<string, string> = {
  Function: "#3B82F6",
  Class: "#8B5CF6",
  Interface: "#14B8A6",
  Method: "#6366F1",
  Module: "#F59E0B",
  Note: "#78716C",
  Section: "#A8A29E",
  Tag: "#22C55E",
};

export const EDGE_COLORS: Record<string, string> = {
  calls: "#9CA3AF",
  imports: "#22C55E",
  extends: "#F97316",
  implements: "#06B6D4",
  wikilink: "#78716C",
  references_code: "#F43F5E",
  tagged_with: "#22C55E",
  cross_repo_declared: "#3B82F6",
  cross_repo_suggested: "#3B82F6",
};

export function kindColor(kind: string, isDark: boolean): string {
  const dark: Record<string, string> = {
    Function: "#3b82f6", Class: "#8b5cf6", Method: "#ec4899",
    Interface: "#06b6d4", Trait: "#84cc16", Enum: "#f59e0b",
    Module: "#f97316", Extension: "#f43f5e", Note: "#a78bfa",
    Section: "#78716c", Tag: "#22c55e",
  };
  const light: Record<string, string> = {
    Function: "#2563eb", Class: "#7c3aed", Method: "#db2777",
    Interface: "#0891b2", Trait: "#65a30d", Enum: "#d97706",
    Module: "#ea580c", Extension: "#e11d48", Note: "#7c3aed",
    Section: "#57534e", Tag: "#16a34a",
  };
  return (isDark ? dark : light)[kind] ?? (isDark ? "#64748b" : "#6b7280");
}

export function kindToColor(kind: string): string {
  const isDark = document.documentElement.classList.contains("dark");
  // API returns "Symbol/Function" — strip the prefix for color lookup
  const short = kind.includes("/") ? kind.split("/").pop()! : kind;
  return kindColor(short, isDark);
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
