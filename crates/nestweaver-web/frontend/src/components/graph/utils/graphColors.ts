export const KIND_COLORS: Record<string, string> = {
  Function: "#1e66f5",
  Class: "#8839ef",
  Interface: "#179299",
  Method: "#7287fd",
  Module: "#df8e1d",
  Note: "#7c7f93",
  Section: "#8c8fa1",
  Tag: "#40a02b",
};

export const EDGE_COLORS: Record<string, string> = {
  overview: "#9ca0b0",
  calls: "#9ca0b0",
  imports: "#9ca0b0",
  extends: "#9ca0b0",
  implements: "#9ca0b0",
  wikilink: "#9ca0b0",
  references_code: "#9ca0b0",
  tagged_with: "#9ca0b0",
  cross_repo_declared: "#9ca0b0",
  cross_repo_suggested: "#9ca0b0",
};

export function kindColor(kind: string, isDark: boolean): string {
  const dark: Record<string, string> = {
    Function: "#89b4fa", Class: "#cba6f7", Method: "#b4befe",
    Interface: "#94e2d5", Trait: "#a6e3a1", Enum: "#f9e2af",
    Module: "#f9e2af", Extension: "#f38ba8", Note: "#9399b2",
    Section: "#a6adc8", Tag: "#a6e3a1", Constant: "#89b4fa",
  };
  const light: Record<string, string> = {
    Function: "#1e66f5", Class: "#8839ef", Method: "#7287fd",
    Interface: "#179299", Trait: "#40a02b", Enum: "#df8e1d",
    Module: "#df8e1d", Extension: "#d20f39", Note: "#7c7f93",
    Section: "#8c8fa1", Tag: "#40a02b", Constant: "#1e66f5",
  };
  return (isDark ? dark : light)[kind] ?? (isDark ? "#585b70" : "#9ca0b0");
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
