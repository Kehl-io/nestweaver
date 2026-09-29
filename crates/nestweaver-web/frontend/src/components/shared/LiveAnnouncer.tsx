import { useStore } from "../../stores";

export function LiveAnnouncer() {
  const liveMessage = useStore((s) => s.liveMessage);

  return (
    <div
      className="sr-only"
      role="status"
      aria-live="polite"
      aria-atomic="true"
      data-testid="live-announcer"
    >
      {liveMessage}
    </div>
  );
}
