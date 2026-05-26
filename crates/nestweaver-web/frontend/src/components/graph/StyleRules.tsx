import { useStore } from "../../stores";

interface Props {
  open: boolean;
}

const rules = [
  { key: "colorByDir", label: "Color by directory" },
  { key: "sizeByCallers", label: "Size by callers" },
  { key: "highlightEntryPoints", label: "Highlight entry points" },
  { key: "highlightHighPageRank", label: "Highlight high PageRank" },
];

export function StyleRules({ open }: Props) {
  const activeStyleRules = useStore((s) => s.activeStyleRules);
  const toggleStyleRule = useStore((s) => s.toggleStyleRule);

  if (!open) return null;

  return (
    <div
      className="w-[200px] rounded border border-[var(--color-border)] bg-[var(--color-surface)] p-3 shadow-md"
      style={{ fontSize: "11px" }}
    >
      <div
        className="mb-2 font-semibold text-[var(--color-text-muted)] uppercase tracking-wide"
        style={{ fontSize: "10px" }}
      >
        Style Rules
      </div>

      {rules.map((rule) => {
        const active = !!activeStyleRules[rule.key];
        return (
          <button
            key={rule.key}
            onClick={() => toggleStyleRule(rule.key)}
            className={`flex w-full items-center justify-between rounded px-2 py-1.5 text-left transition-colors ${
              active
                ? "bg-blue-50 text-blue-700 dark:bg-blue-900/40 dark:text-blue-300"
                : "text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] dark:hover:bg-white/5"
            }`}
          >
            <span>{rule.label}</span>
            {active && (
              <span className="ml-2 text-blue-600 dark:text-blue-300" aria-label="active">
                ✓
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}
