import { useRef, useCallback, type FormEvent, type KeyboardEvent } from "react";
import { useStore } from "../../stores";
import { api } from "../../api/client";

export function LlmQueryBar() {
  const open = useStore((s) => s.llmBarOpen);
  const query = useStore((s) => s.llmQuery);
  const loading = useStore((s) => s.llmLoading);
  const error = useStore((s) => s.llmError);

  const inputRef = useRef<HTMLInputElement>(null);

  const close = useCallback(() => {
    useStore.getState().closeLlmBar();
  }, []);

  const handleSubmit = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      const q = useStore.getState().llmQuery.trim();
      if (!q) return;

      const { setLlmLoading, setLlmResult, setLlmError, setSeeds, setGraphMode, closeLlmBar } =
        useStore.getState();

      setLlmLoading(true);
      setLlmError(null);

      try {
        const result = await api.llmQuery(q, 4000);
        setLlmResult(result);
        setSeeds(result.seeds);
        setGraphMode("context");
        closeLlmBar();
      } catch (err) {
        setLlmError(err instanceof Error ? err.message : "Query failed");
      } finally {
        setLlmLoading(false);
      }
    },
    [],
  );

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close();
      }
    },
    [close],
  );

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex justify-center bg-black/30"
      onClick={close}
      style={{ paddingTop: "15vh" }}
    >
      <div
        className="h-fit w-full max-w-lg rounded-lg bg-[var(--color-surface)] p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <form onSubmit={handleSubmit}>
          <label
            htmlFor="llm-query-input"
            className="mb-1 block text-xs font-medium text-[var(--color-text-muted)]"
          >
            Ask:
          </label>
          <input
            ref={inputRef}
            id="llm-query-input"
            type="text"
            autoFocus
            className="w-full rounded border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-2 text-sm outline-none focus:ring-2 focus:ring-blue-400"
            placeholder="e.g. Show me the authentication flow"
            value={query}
            onChange={(e) => useStore.getState().setLlmQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            disabled={loading}
          />
        </form>

        {loading && (
          <p className="mt-2 text-sm text-[var(--color-text-muted)]">
            Thinking...
          </p>
        )}

        {error && (
          <p className="mt-2 text-sm text-red-600">{error}</p>
        )}

        <p className="mt-3 text-[10px] text-[var(--color-text-muted)]">
          Natural language &rarr; PPR subgraph
        </p>
      </div>
    </div>
  );
}
