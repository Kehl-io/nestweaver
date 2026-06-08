import { useEffect, useState, type ReactNode } from "react";

interface CollapsibleProps {
  title: string;
  count?: number;
  defaultOpen?: boolean;
  active?: boolean;
  children: ReactNode;
}

export function Collapsible({
  title,
  count,
  defaultOpen = true,
  active = false,
  children,
}: CollapsibleProps) {
  const [open, setOpen] = useState(defaultOpen);

  useEffect(() => {
    if (active) setOpen(true);
  }, [active]);

  return (
    <div
      className={
        active
          ? "rounded border border-blue-500/40 bg-blue-500/5"
          : undefined
      }
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-1 px-2 py-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
      >
        <span className="w-3 text-center">{open ? "▾" : "▸"}</span>
        <span>{title}</span>
        {count != null && (
          <span className="ml-auto rounded-full bg-[var(--color-border)] px-1.5 py-0.5 text-[10px] font-normal leading-none">
            {count}
          </span>
        )}
      </button>
      {open && <div>{children}</div>}
    </div>
  );
}
