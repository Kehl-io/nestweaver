import * as Dialog from "@radix-ui/react-dialog";
import { useCallback, type FormEvent } from "react";
import { useStore } from "../../stores";
import { api } from "../../api/client";

function getGraphFocusFallback() {
  if (typeof document === "undefined") return null;
  return document.querySelector<HTMLElement>(
    '[role="application"][aria-label="Code knowledge graph"]',
  );
}

function restoreFocus(target: HTMLElement | null) {
  const fallback = getGraphFocusFallback();
  const nextTarget =
    target &&
    target.isConnected &&
    target !== document.body &&
    target !== document.documentElement
      ? target
      : fallback;

  nextTarget?.focus({ preventScroll: true });

  if (
    fallback &&
    nextTarget !== fallback &&
    document.activeElement !== nextTarget
  ) {
    fallback.focus({ preventScroll: true });
  }
}

export function LlmQueryBar() {
  const open = useStore((s) => s.llmBarOpen);
  const query = useStore((s) => s.llmQuery);
  const loading = useStore((s) => s.llmLoading);
  const error = useStore((s) => s.llmError);
  const clearFocusReturnTarget = useStore((s) => s.clearLlmFocusReturnTarget);

  const close = useCallback(() => {
    useStore.getState().closeLlmBar();
  }, []);

  const handleCloseAutoFocus = useCallback(
    (event: Event) => {
      event.preventDefault();
      restoreFocus(useStore.getState().getLlmFocusReturnTarget());
      clearFocusReturnTarget();
    },
    [clearFocusReturnTarget],
  );

  const handleSubmit = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      const q = useStore.getState().llmQuery.trim();
      if (!q) return;

      const { setLlmLoading, setLlmResult, setLlmError, setSeeds, setGraphMode, closeLlmBar, notify } =
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
        const message = err instanceof Error ? err.message : "Query failed";
        setLlmError(message);
        notify({ kind: "error", title: "Query failed", message });
      } finally {
        setLlmLoading(false);
      }
    },
    [],
  );

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) close();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/30" />
        <Dialog.Content
          aria-describedby="llm-query-description"
          className="fixed left-1/2 top-[15vh] z-50 h-fit w-[min(32rem,calc(100vw-2rem))] -translate-x-1/2 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] p-4 text-[var(--color-text)] shadow-xl focus:outline-none"
          onCloseAutoFocus={handleCloseAutoFocus}
        >
          <Dialog.Title className="sr-only">Ask</Dialog.Title>
          <Dialog.Description
            id="llm-query-description"
            className="mb-3 text-xs text-[var(--color-text-muted)]"
          >
            Natural language to PPR subgraph
          </Dialog.Description>

          <form onSubmit={handleSubmit}>
            <label
              htmlFor="llm-query-input"
              className="mb-1 block text-xs font-medium text-[var(--color-text-muted)]"
            >
              Ask
            </label>
            <input
              id="llm-query-input"
              type="text"
              autoFocus
              aria-label="Ask"
              className="w-full rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2 text-sm outline-none focus:ring-2 focus:ring-[var(--color-graph-selection)] disabled:cursor-not-allowed disabled:opacity-60"
              placeholder="e.g. Show me the authentication flow"
              value={query}
              onChange={(e) => useStore.getState().setLlmQuery(e.target.value)}
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

        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
