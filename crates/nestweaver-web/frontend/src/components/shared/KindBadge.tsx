const kindConfig: Record<string, { color: string; abbr: string }> = {
  Function: { color: "var(--color-fn)", abbr: "fn" },
  Class: { color: "var(--color-cls)", abbr: "C" },
  Interface: { color: "var(--color-ifc)", abbr: "I" },
  Method: { color: "var(--color-method)", abbr: "m" },
  Module: { color: "var(--color-module)", abbr: "M" },
  Note: { color: "var(--color-note)", abbr: "N" },
  Section: { color: "var(--color-section)", abbr: "S" },
  Tag: { color: "var(--color-tag)", abbr: "#" },
  General: { color: "var(--color-note)", abbr: "G" },
};

const fallback = { color: "var(--color-text-muted)", abbr: "?" };

export function KindBadge({ kind }: { kind: string }) {
  const cfg = kindConfig[kind] ?? fallback;
  return (
    <span
      className="inline-flex items-center justify-center rounded px-1.5 py-0.5 text-[10px] font-semibold leading-none text-white"
      style={{ backgroundColor: cfg.color }}
    >
      {cfg.abbr}
    </span>
  );
}
