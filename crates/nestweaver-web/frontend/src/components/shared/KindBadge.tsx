const kindConfig = new Map([
  ["function", { label: "Function", abbr: "fn", color: "fn" }],
  ["class", { label: "Class", abbr: "C", color: "cls" }],
  ["interface", { label: "Interface", abbr: "I", color: "ifc" }],
  ["method", { label: "Method", abbr: "m", color: "method" }],
  ["module", { label: "Module", abbr: "M", color: "module" }],
  ["trait", { label: "Trait", abbr: "Tr", color: "ifc" }],
  ["enum", { label: "Enum", abbr: "En", color: "cls" }],
  ["extension", { label: "Extension", abbr: "Ext", color: "module" }],
  ["constant", { label: "Constant", abbr: "Const", color: "module" }],
  ["property", { label: "Property", abbr: "Prop", color: "method" }],
  ["typealias", { label: "Type alias", abbr: "Type", color: "ifc" }],
  ["variable", { label: "Variable", abbr: "Var", color: "method" }],
  ["struct", { label: "Struct", abbr: "St", color: "cls" }],
  ["symbol", { label: "Symbol", abbr: "Sym", color: "text-muted" }],
  ["file", { label: "File", abbr: "File", color: "text-muted" }],
  ["note", { label: "Note", abbr: "N", color: "note" }],
  ["heading", { label: "Heading", abbr: "H", color: "section" }],
  ["section", { label: "Section", abbr: "S", color: "section" }],
  ["tag", { label: "Tag", abbr: "#", color: "tag" }],
  ["general", { label: "General", abbr: "G", color: "note" }],
]);

export function KindBadge({ kind }: { kind?: string | null }) {
  const value = kind?.trim() ?? "";
  const cfg = kindConfig.get(value.toLowerCase().replace(/[\s_-]+/g, ""));
  const label = cfg?.label ?? (
    value && value.toLowerCase() !== "unknown" && value !== "?"
      ? value.replace(/([a-z\d])([A-Z])/g, "$1 $2").replace(/[_-]+/g, " ")
      : "Unknown kind"
  );
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className="inline-block max-w-24 truncate rounded px-1.5 py-0.5 text-center text-[10px] font-semibold leading-none text-white"
      style={{ backgroundColor: `var(--color-${cfg?.color ?? "text-muted"})` }}
    >
      {cfg?.abbr ?? label}
    </span>
  );
}
