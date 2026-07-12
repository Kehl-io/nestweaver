import * as Select from "@radix-ui/react-select";
import { ChevronDown } from "lucide-react";
import type { ScopeFilter } from "../../api/types";

const SCOPE_OPTIONS: { value: ScopeFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "code_only", label: "Code only" },
  { value: "notes_only", label: "Notes only" },
];

interface ScopeSelectProps {
  value: ScopeFilter;
  onChange: (value: ScopeFilter) => void;
  label?: string;
  compact?: boolean;
}

export function ScopeSelect({
  value,
  onChange,
  label = "Search scope",
  compact = false,
}: ScopeSelectProps) {
  const selectedLabel =
    SCOPE_OPTIONS.find((option) => option.value === value)?.label ?? "All";

  return (
    <div className={compact ? "shrink-0" : "w-full"}>
      <Select.Root value={value} onValueChange={(next) => onChange(next as ScopeFilter)}>
        <Select.Trigger
          aria-label={label}
          title={`${label}: ${selectedLabel}`}
          className={`inline-flex items-center justify-between gap-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] text-xs text-[var(--color-text)] outline-none transition-colors hover:bg-[var(--color-surface)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] focus-visible:ring-offset-1 focus-visible:ring-offset-[var(--color-surface)] ${
            compact ? "h-8 w-[6.25rem] px-2" : "h-8 w-full px-2"
          }`}
        >
          <Select.Value />
          <Select.Icon asChild>
            <ChevronDown className="h-3.5 w-3.5 shrink-0 text-[var(--color-text-muted)]" />
          </Select.Icon>
        </Select.Trigger>
        <Select.Portal>
          <Select.Content
            position="popper"
            sideOffset={4}
            className="z-[100] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] py-1 text-xs text-[var(--color-text)] shadow-xl"
          >
            <Select.Viewport>
              {SCOPE_OPTIONS.map((option) => (
                <Select.Item
                  key={option.value}
                  value={option.value}
                  className="relative cursor-default select-none px-3 py-1.5 outline-none data-[highlighted]:bg-[var(--color-surface-alt)] data-[highlighted]:text-[var(--color-text)] data-[state=checked]:text-[var(--color-graph-selection)]"
                >
                  <Select.ItemText>{option.label}</Select.ItemText>
                </Select.Item>
              ))}
            </Select.Viewport>
          </Select.Content>
        </Select.Portal>
      </Select.Root>
    </div>
  );
}
