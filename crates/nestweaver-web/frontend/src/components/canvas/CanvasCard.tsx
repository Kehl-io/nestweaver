import { useCallback, useEffect } from "react";
import { useStore } from "../../stores";
import { KindBadge } from "../shared/KindBadge";
import type { CanvasElement } from "../../stores/contentSlice";

interface CanvasCardProps {
  element: CanvasElement;
}

export function CanvasCard({ element }: CanvasCardProps) {
  const selectedElementId = useStore((s) => s.canvas.selectedElementId);
  const selectElement = useStore((s) => s.selectElement);
  const setDragged = useStore((s) => s.setDragged);
  const updateElement = useStore((s) => s.updateElement);

  const isSelected = selectedElementId === element.id;

  const handleMouseDown = useCallback(
    (e: React.MouseEvent) => {
      e.stopPropagation();
      selectElement(element.id);
      setDragged(element.id);

      const startX = e.clientX;
      const startY = e.clientY;
      const origX = element.x;
      const origY = element.y;

      const onMouseMove = (ev: MouseEvent) => {
        const dx = ev.clientX - startX;
        const dy = ev.clientY - startY;
        updateElement(element.id, { x: origX + dx, y: origY + dy });
      };

      const onMouseUp = () => {
        setDragged(null);
        window.removeEventListener("mousemove", onMouseMove);
        window.removeEventListener("mouseup", onMouseUp);
      };

      window.addEventListener("mousemove", onMouseMove);
      window.addEventListener("mouseup", onMouseUp);
    },
    [element.id, element.x, element.y, selectElement, setDragged, updateElement],
  );

  useEffect(() => {
    return () => {
      setDragged(null);
    };
  }, [setDragged]);

  const renderContent = () => {
    switch (element.type) {
      case "symbol":
        return (
          <div className="flex items-center gap-2 p-3">
            <KindBadge kind="Function" />
            <span className="text-sm font-medium truncate">
              {element.content ?? element.uid ?? "Symbol"}
            </span>
          </div>
        );
      case "note":
        return (
          <div className="flex items-center gap-2 p-3">
            <KindBadge kind="Note" />
            <span className="text-sm font-medium truncate">
              {element.content ?? "Note"}
            </span>
          </div>
        );
      case "text":
        return (
          <div className="p-3">
            <p className="text-sm text-[var(--color-text)]">
              {element.content ?? ""}
            </p>
          </div>
        );
      case "image":
        return (
          <div className="flex items-center justify-center p-3 text-[var(--color-text-muted)]">
            <svg
              className="w-8 h-8"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={1.5}
                d="M4 16l4.586-4.586a2 2 0 012.828 0L16 16m-2-2l1.586-1.586a2 2 0 012.828 0L20 14m-6-6h.01M6 20h12a2 2 0 002-2V6a2 2 0 00-2-2H6a2 2 0 00-2 2v12a2 2 0 002 2z"
              />
            </svg>
          </div>
        );
      default:
        return null;
    }
  };

  return (
    <div
      onMouseDown={handleMouseDown}
      className={`absolute cursor-grab select-none rounded-lg border bg-[var(--color-bg-secondary)] shadow-sm transition-shadow ${
        isSelected
          ? "border-blue-500 ring-2 ring-blue-500/30"
          : "border-[var(--color-border)] hover:border-[var(--color-border-hover)]"
      }`}
      style={{
        left: element.x,
        top: element.y,
        width: element.width,
        height: element.height,
      }}
    >
      {renderContent()}
    </div>
  );
}
