import { useEffect } from "react";
import { CheckCircle2, CircleAlert, Info, TriangleAlert, X } from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { useStore } from "../../stores";
import type { NotificationKind } from "../../stores/notificationSlice";

const kindMeta: Record<
  NotificationKind,
  { Icon: LucideIcon; label: string; border: string; icon: string; dismissAfterMs: number }
> = {
  info: {
    Icon: Info,
    label: "Info",
    border: "border-[var(--color-border)]",
    icon: "text-[var(--color-text-muted)]",
    dismissAfterMs: 5_000,
  },
  success: {
    Icon: CheckCircle2,
    label: "Success",
    border: "border-green-500/50",
    icon: "text-green-500",
    dismissAfterMs: 5_000,
  },
  warning: {
    Icon: TriangleAlert,
    label: "Warning",
    border: "border-amber-500/60",
    icon: "text-amber-500",
    dismissAfterMs: 8_000,
  },
  error: {
    Icon: CircleAlert,
    label: "Error",
    border: "border-red-500/60",
    icon: "text-red-500",
    dismissAfterMs: 10_000,
  },
};

export function ToastViewport() {
  const notifications = useStore((s) => s.notifications);
  const dismissNotification = useStore((s) => s.dismissNotification);
  const visibleNotifications = notifications.slice(0, 4);

  useEffect(() => {
    if (notifications.length === 0) return;
    const now = Date.now();
    const timers = notifications.map((notification) => {
      const expiresAt = notification.createdAt + kindMeta[notification.kind].dismissAfterMs;
      return setTimeout(
        () => useStore.getState().dismissNotification(notification.id),
        Math.max(0, expiresAt - now),
      );
    });
    return () => timers.forEach(clearTimeout);
  }, [notifications]);

  if (visibleNotifications.length === 0) return null;

  return (
    <div
      role="region"
      aria-label="Notifications"
      className="fixed bottom-8 right-3 z-50 flex w-[min(24rem,calc(100vw-1.5rem))] flex-col gap-2"
    >
      {visibleNotifications.map((notification) => {
        const meta = kindMeta[notification.kind];
        return (
          <div
            key={notification.id}
            className={`rounded-md border bg-[var(--color-surface)] px-3 py-2 text-[var(--color-text)] shadow-xl ${meta.border}`}
          >
            <div className="flex items-start gap-3">
              <meta.Icon size={16} aria-hidden="true" className={`mt-0.5 shrink-0 ${meta.icon}`} />
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm font-semibold">
                  <span className="sr-only">{meta.label}: </span>
                  {notification.title}
                </p>
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
        );
      })}
    </div>
  );
}
