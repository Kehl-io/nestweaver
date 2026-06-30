import { useCallback, useMemo } from "react";

export function useAdminApi() {
  const token = sessionStorage.getItem("admin_token");

  const get = useCallback(
    async <T>(path: string): Promise<T> => {
      const res = await fetch(`/admin/api${path}`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (res.status === 401) {
        sessionStorage.removeItem("admin_token");
        window.location.href = "/admin/login";
        throw new Error("Unauthorized");
      }
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      return res.json();
    },
    [token],
  );

  const post = useCallback(
    async <T = unknown>(path: string, body?: unknown): Promise<T> => {
      const res = await fetch(`/admin/api${path}`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${token}`,
          "Content-Type": "application/json",
        },
        body: body ? JSON.stringify(body) : undefined,
      });
      if (res.status === 401) {
        sessionStorage.removeItem("admin_token");
        window.location.href = "/admin/login";
        throw new Error("Unauthorized");
      }
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      return res.json();
    },
    [token],
  );

  const del = useCallback(
    async (path: string): Promise<void> => {
      const res = await fetch(`/admin/api${path}`, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${token}` },
      });
      if (res.status === 401) {
        sessionStorage.removeItem("admin_token");
        window.location.href = "/admin/login";
        throw new Error("Unauthorized");
      }
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    },
    [token],
  );

  return useMemo(() => ({ get, post, del }), [get, post, del]);
}
