import { useEffect, useState } from "react";
import { useStore } from "../../stores";

/**
 * Polite live region. Screen readers only speak a change, so each
 * announcement first empties the region and then writes the message a
 * moment later (after the empty state has rendered, which a same-frame
 * write would not guarantee); an identical repeat is therefore spoken again.
 */
export function LiveAnnouncer() {
  const liveMessage = useStore((s) => s.liveMessage);
  const liveMessageId = useStore((s) => s.liveMessageId);
  const [text, setText] = useState(liveMessage);

  useEffect(() => {
    setText("");
    const timer = setTimeout(() => setText(liveMessage), 50);
    return () => clearTimeout(timer);
  }, [liveMessage, liveMessageId]);

  return (
    <div
      className="sr-only"
      role="status"
      aria-live="polite"
      aria-atomic="true"
      data-testid="live-announcer"
    >
      {text}
    </div>
  );
}
