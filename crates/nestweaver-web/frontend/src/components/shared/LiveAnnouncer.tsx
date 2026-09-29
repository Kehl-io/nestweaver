import { useStore } from "../../stores";

/**
 * Polite live region. Each announcement renders a fresh text node, keyed on
 * the store's liveMessageId, so React replaces the node even when the text
 * is identical to the previous message. Screen readers announce the added
 * node, so a repeated result is spoken again, with no timer involved.
 */
export function LiveAnnouncer() {
  const liveMessage = useStore((s) => s.liveMessage);
  const liveMessageId = useStore((s) => s.liveMessageId);

  return (
    <div
      className="sr-only"
      role="status"
      aria-live="polite"
      aria-atomic="true"
      data-testid="live-announcer"
    >
      {liveMessage && (
        <span key={liveMessageId} data-message-id={liveMessageId}>
          {liveMessage}
        </span>
      )}
    </div>
  );
}
