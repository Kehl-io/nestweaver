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
    <div className="flex h-full flex-col border-r border-[var(--color-border)] bg-[var(--color-surface)]">
      <div className="flex border-b border-[var(--color-border)]">
        {tabs.map((t) => (
          <button
            key={t.key}
            type="button"
            onClick={() => setExplorerTab(t.key)}
            className={`flex-1 px-3 py-2 text-xs font-medium transition-colors ${
              explorerTab === t.key
                ? "border-b-2 border-blue-500 text-blue-600"
                : "border-b-2 border-transparent text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
            }`}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div className="flex-1 overflow-hidden">
        {explorerTab === "files" && <FilesTab />}
        {explorerTab === "symbols" && <SymbolsTab />}
        {explorerTab === "notes" && <NotesTab />}
      </div>
    </div>
  );
}
