import type { StateCreator } from "zustand";
import type { StoreState } from "./index";

export type NotificationKind = "info" | "success" | "warning" | "error";

export interface Notification {
  id: string;
  kind: NotificationKind;
  title: string;
  message?: string;
  createdAt: number;
}

export interface NotifyInput {
  kind: NotificationKind;
  title: string;
  message?: string;
}

export interface NotificationSlice {
  notifications: Notification[];
  liveMessage: string;
  notify: (input: NotifyInput) => string;
  dismissNotification: (id: string) => void;
  announce: (message: string) => void;
}

function createNotificationId() {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }

  return `notification-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

function formatLiveMessage(input: NotifyInput) {
  return input.message ? `${input.title}. ${input.message}` : input.title;
}

export const createNotificationSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  NotificationSlice
> = (set) => ({
  notifications: [],
  liveMessage: "",
  notify: (input) => {
    const id = createNotificationId();
    set((s) => {
      s.notifications.unshift({
        id,
        kind: input.kind,
        title: input.title,
        message: input.message,
        createdAt: Date.now(),
      });
      s.liveMessage = formatLiveMessage(input);
    });
    return id;
  },
  dismissNotification: (id) =>
    set((s) => {
      s.notifications = s.notifications.filter((notification) => notification.id !== id);
    }),
  announce: (message) =>
    set((s) => {
      s.liveMessage = message;
    }),
});
