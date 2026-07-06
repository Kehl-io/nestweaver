import { X } from "lucide-react";
import { useStore } from "../../stores";
import type { NotificationKind } from "../../stores/notificationSlice";

const kindClasses: Record<NotificationKind, string> = {
  info: "border-[var(--color-border)]",
  success: "border-green-500/50",
  warning: "border-amber-500/60",
  error: "border-red-500/60",
};

export function ToastViewport() {
  const notifications = useStore((s) => s.notifications);
  const dismissNotification = useStore((s) => s.dismissNotification);
  const visibleNotifications = notifications.slice(0, 4);

  if (visibleNotifications.length === 0) return null;

  return (
    <div
      role="region"
      aria-label="Notifications"
      className="fixed bottom-8 right-3 z-50 flex w-[min(24rem,calc(100vw-1.5rem))] flex-col gap-2"
    >
      {visibleNotifications.map((notification) => (
        <div
          key={notification.id}
          className={`rounded-md border bg-[var(--color-surface)] px-3 py-2 text-[var(--color-text)] shadow-xl ${kindClasses[notification.kind]}`}
        >
          <div className="flex items-start gap-3">
            <div className="min-w-0 flex-1">
              <p className="truncate text-sm font-semibold">{notification.title}</p>
              {notification.message && (
                <p className="mt-0.5 line-clamp-3 text-xs leading-5 text-[var(--color-text-muted)]">
                  {notification.message}
                </p>
              )}
            </div>
            <button
              type="button"
              aria-label={`Dismiss ${notification.title}`}
              onClick={() => dismissNotification(notification.id)}
              className="flex h-7 w-7 shrink-0 items-center justify-center rounded border border-[var(--color-border)] text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              <X size={14} aria-hidden="true" />
            </button>
          </div>
        </div>
      ))}
    </div>
  );
}
