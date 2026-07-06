import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Monitor, Moon, Sun } from "lucide-react";
import type { ComponentType } from "react";
import { useStore } from "../../stores";

type Theme = "system" | "light" | "dark";

const THEME_OPTIONS: {
  value: Theme;
  label: string;
  Icon: ComponentType<{ className?: string }>;
}[] = [
  { value: "system", label: "System", Icon: Monitor },
  { value: "light", label: "Light", Icon: Sun },
  { value: "dark", label: "Dark", Icon: Moon },
];

const THEME_LABELS: Record<Theme, string> = {
  system: "System",
  light: "Light",
  dark: "Dark",
};

export function ThemeMenu() {
  const theme = useStore((s) => s.theme);
  const setTheme = useStore((s) => s.setTheme);
  const CurrentIcon =
    THEME_OPTIONS.find((option) => option.value === theme)?.Icon ?? Monitor;

  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          aria-label={`Theme: ${THEME_LABELS[theme]}`}
          title={`Theme: ${THEME_LABELS[theme]}`}
          className="flex h-8 w-8 shrink-0 items-center justify-center rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] text-[var(--color-text-muted)] outline-none transition-colors hover:bg-[var(--color-surface)] hover:text-[var(--color-text)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] focus-visible:ring-offset-1 focus-visible:ring-offset-[var(--color-surface)]"
        >
          <CurrentIcon className="h-4 w-4" aria-hidden="true" />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={6}
          className="z-[100] min-w-36 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] p-1 text-xs text-[var(--color-text)] shadow-xl"
        >
          <DropdownMenu.RadioGroup
            value={theme}
            onValueChange={(next) => setTheme(next as Theme)}
          >
            {THEME_OPTIONS.map(({ value, label, Icon }) => (
              <DropdownMenu.RadioItem
                key={value}
                value={value}
                className="flex cursor-default select-none items-center gap-2 rounded px-2 py-1.5 outline-none data-[highlighted]:bg-[var(--color-surface-alt)] data-[state=checked]:text-[var(--color-graph-selection)]"
              >
                <Icon className="h-3.5 w-3.5" aria-hidden="true" />
                <span>{label}</span>
              </DropdownMenu.RadioItem>
            ))}
          </DropdownMenu.RadioGroup>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
