import { useStore } from "../../stores";
import { CanvasToolbar } from "./CanvasToolbar";
import { CanvasCard } from "./CanvasCard";

export function CanvasView() {
  const elements = useStore((s) => s.canvas.elements);
  const connections = useStore((s) => s.canvas.connections);
  const sections = useStore((s) => s.canvas.sections);
  const selectElement = useStore((s) => s.selectElement);

  const handleBackgroundClick = () => {
    selectElement(null);
  };

  const getElementCenter = (id: string) => {
    const el = elements.find((e) => e.id === id);
    if (!el) return { x: 0, y: 0 };
    return { x: el.x + el.width / 2, y: el.y + el.height / 2 };
  };

  return (
    <div className="flex flex-col h-full">
      <CanvasToolbar />

      <div
        className="relative flex-1 overflow-auto bg-[var(--color-bg)]"
        onClick={handleBackgroundClick}
      >
        {/* Connection lines */}
        <svg className="absolute inset-0 w-full h-full pointer-events-none">
          <defs>
            <marker
              id="arrowhead"
              markerWidth="10"
              markerHeight="7"
              refX="10"
              refY="3.5"
              orient="auto"
            >
              <polygon
                points="0 0, 10 3.5, 0 7"
                fill="var(--color-text-muted)"
              />
            </marker>
          </defs>
          {connections.map((conn) => {
            const from = getElementCenter(conn.fromId);
            const to = getElementCenter(conn.toId);
            return (
              <line
                key={conn.id}
                x1={from.x}
                y1={from.y}
                x2={to.x}
                y2={to.y}
                stroke="var(--color-text-muted)"
                strokeWidth={1.5}
                markerEnd="url(#arrowhead)"
              />
            );
          })}
        </svg>

        {/* Sections */}
        {sections.map((section) => (
          <div
            key={section.id}
            className="absolute rounded-lg border-2 border-dashed pointer-events-none"
            style={{
              left: section.x,
              top: section.y,
              width: section.width,
              height: section.height,
              borderColor: section.color,
            }}
          >
            <span
              className="absolute -top-3 left-3 px-1 text-xs font-medium bg-[var(--color-bg)]"
              style={{ color: section.color }}
            >
              {section.name}
            </span>
          </div>
        ))}

        {/* Elements */}
        {elements.map((el) => (
          <CanvasCard key={el.id} element={el} />
        ))}

        {/* Empty state */}
        {elements.length === 0 && sections.length === 0 && (
          <div className="absolute inset-0 flex items-center justify-center">
            <p className="text-sm text-[var(--color-text-muted)]">
              Empty canvas. Use the toolbar to add elements.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}
