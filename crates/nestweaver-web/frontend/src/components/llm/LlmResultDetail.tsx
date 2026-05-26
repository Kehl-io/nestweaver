import { useStore } from "../../stores";

export function LlmResultDetail() {
  const llmResult = useStore((s) => s.llmResult);
  const clearLlm = useStore((s) => s.clearLlm);

  if (!llmResult) return null;

  const nodeCount =
    (llmResult.context?.seeds?.length ?? 0) +
    (llmResult.context?.connected?.length ?? 0);

  return (
    <div className="border-b border-[var(--color-border)] p-4">
      <div className="mb-2 flex items-center justify-between">
        <h3 className="text-sm font-semibold">LLM Query</h3>
        <button
          type="button"
          className="rounded px-2 py-0.5 text-xs text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)]"
          onClick={clearLlm}
        >
          Clear
        </button>
      </div>

      <p className="mb-3 text-sm text-[var(--color-text)]">
        {llmResult.explanation}
      </p>

      <div className="mb-2">
        <h4 className="mb-1 text-xs font-medium text-[var(--color-text-muted)]">
          Seeds extracted
        </h4>
        <div className="flex flex-wrap gap-1">
          {llmResult.seeds.map((seed) => (
            <span
              key={seed}
              className="inline-block rounded-full bg-blue-100 px-2 py-0.5 text-xs text-blue-800"
            >
              {seed}
            </span>
          ))}
        </div>
      </div>

      <p className="text-xs text-[var(--color-text-muted)]">
        {nodeCount} node{nodeCount !== 1 ? "s" : ""} in context
      </p>
    </div>
  );
}
