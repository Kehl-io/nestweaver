import { useStore } from "../../stores";
import type { ExplorerTab } from "../../stores/panelSlice";
import { FilesTab } from "./FilesTab";
import { NotesTab } from "./NotesTab";
import { SymbolsTab } from "./SymbolsTab";

const tabs: { key: ExplorerTab; label: string }[] = [
  { key: "files", label: "Files" },
  { key: "symbols", label: "Symbols" },
  { key: "notes", label: "Notes" },
];

export function ExplorerPanel() {
  const explorerTab = useStore((s) => s.explorerTab);
  const setExplorerTab = useStore((s) => s.setExplorerTab);

  return (
    <div data-testid="explorer-panel" className="flex h-full flex-col border-r border-[var(--color-border)] bg-[var(--color-surface)]">
      {/* WAI-ARIA tabs: one Tab stop, arrows switch tabs (nw-565). */}
      <div
        role="tablist"
        aria-label="Explorer"
        className="flex border-b border-[var(--color-border)]"
        onKeyDown={(event) => {
          const index = tabs.findIndex((t) => t.key === explorerTab);
          let next = -1;
          if (event.key === "ArrowRight") next = (index + 1) % tabs.length;
          else if (event.key === "ArrowLeft") next = (index - 1 + tabs.length) % tabs.length;
          else if (event.key === "Home") next = 0;
          else if (event.key === "End") next = tabs.length - 1;
          if (next < 0) return;
          event.preventDefault();
          setExplorerTab(tabs[next].key);
          document.getElementById(`explorer-tab-${tabs[next].key}`)?.focus();
        }}
      >
        {tabs.map((t) => (
          <button
            key={t.key}
            id={`explorer-tab-${t.key}`}
            type="button"
            role="tab"
            aria-selected={explorerTab === t.key}
            aria-controls="explorer-tabpanel"
            tabIndex={explorerTab === t.key ? 0 : -1}
            onClick={() => setExplorerTab(t.key)}
            className={`flex-1 px-3 py-2 text-xs font-medium outline-none transition-colors focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-[var(--color-graph-selection)] ${
              explorerTab === t.key
                ? "border-b-2 border-blue-500 text-blue-600"
                : "border-b-2 border-transparent text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
            }`}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div
        id="explorer-tabpanel"
        role="tabpanel"
        aria-labelledby={`explorer-tab-${explorerTab}`}
        className="flex-1 overflow-hidden"
      >
        {explorerTab === "files" && <FilesTab />}
        {explorerTab === "symbols" && <SymbolsTab />}
        {explorerTab === "notes" && <NotesTab />}
      </div>
    </div>
  );
}
