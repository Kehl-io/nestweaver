import { useStore } from "../../stores";

export function LiveAnnouncer() {
  const liveMessage = useStore((s) => s.liveMessage);

  return (
    <div className="sr-only" aria-live="polite" aria-atomic="true">
      {liveMessage}
    </div>
  );
}
