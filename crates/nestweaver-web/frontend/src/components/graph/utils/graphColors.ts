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

export function kindToColor(kind: string): string {
  return KIND_COLORS[kind] || "#9CA3AF";
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

export function relevanceToSaturation(
  relevance: number,
  maxRelevance: number,
): number {
  if (maxRelevance <= 0) return 0;
  const normalized = relevance / maxRelevance;
  return 1 - normalized;
}
