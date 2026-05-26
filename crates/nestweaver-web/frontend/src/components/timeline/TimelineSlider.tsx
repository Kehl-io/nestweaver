import { useEffect } from "react";
import { useStore } from "../../stores";
import { api } from "../../api/client";

export function TimelineSlider() {
  const entries = useStore((s) => s.timelineEntries);
  const position = useStore((s) => s.timelinePosition);
  const playing = useStore((s) => s.timelinePlaying);
  const setEntries = useStore((s) => s.setTimelineEntries);
  const setPosition = useStore((s) => s.setTimelinePosition);
  const setPlaying = useStore((s) => s.setTimelinePlaying);

  useEffect(() => {
    api
      .repos()
      .then(async (repos) => {
        if (repos.length > 0) {
          try {
            const res = await fetch(`/api/v1/timeline/${repos[0].uid}`);
            const data = await res.json();
            if (Array.isArray(data)) setEntries(data);
          } catch {
            // timeline endpoint may not exist yet
          }
        }
      })
      .catch(() => {});
  }, [setEntries]);

  useEffect(() => {
    if (!playing || entries.length === 0) return;
    const timer = setInterval(() => {
      const current = useStore.getState().timelinePosition;
      if (current >= entries.length - 1) {
        setPlaying(false);
        return;
      }
      setPosition(current + 1);
    }, 500);
    return () => clearInterval(timer);
  }, [playing, entries.length, setPosition, setPlaying]);

  if (entries.length === 0) return null;

  const current = entries[position];
  return (
    <div className="flex h-10 shrink-0 items-center gap-2 border-t border-[var(--color-border)] bg-[var(--color-surface)] px-3">
      <button
        onClick={() => setPlaying(!playing)}
        className="flex h-6 w-6 items-center justify-center rounded border border-[var(--color-border)] text-xs"
      >
        {playing ? "⏸" : "▶"}
      </button>
      <input
        type="range"
        min={0}
        max={entries.length - 1}
        value={position}
        onChange={(e) => setPosition(Number(e.target.value))}
        className="h-1 flex-1"
      />
      <span className="w-28 truncate text-right text-[10px] text-[var(--color-text-muted)]">
        {current?.message?.slice(0, 30) ||
          `${position + 1}/${entries.length}`}
      </span>
    </div>
  );
}
