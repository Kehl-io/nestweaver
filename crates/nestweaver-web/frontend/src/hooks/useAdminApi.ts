import { useCallback, useMemo } from "react";

export function useAdminApi() {
  const token = sessionStorage.getItem("admin_token");

  // Keep authentication and session expiry handling shared even for admin
  // operations, such as device approval, outside the /admin/api router.
  const send = useCallback(
    async (path: string, init: RequestInit = {}): Promise<Response> => {
      const headers = new Headers(init.headers);
      headers.set("Authorization", `Bearer ${token}`);
      const res = await fetch(path, { ...init, headers });
      if (res.status === 401) {
        sessionStorage.removeItem("admin_token");
        window.location.href = "/admin/login";
        throw new Error("Unauthorized");
      }
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      return res;
    },
    [token],
  );

  const postJson = useCallback(
    async <T = unknown>(path: string, body?: unknown): Promise<T> => {
      const res = await send(path, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: body ? JSON.stringify(body) : undefined,
      });
      return res.json();
    },
    [send],
  );

  const get = useCallback(
    async <T>(path: string): Promise<T> => {
      const res = await send(`/admin/api${path}`);
      return res.json();
    },
    [send],
  );

  const post = useCallback(
    <T = unknown>(path: string, body?: unknown): Promise<T> =>
      postJson<T>(`/admin/api${path}`, body),
    [postJson],
  );

  const del = useCallback(
    async (path: string): Promise<void> => {
      await send(`/admin/api${path}`, { method: "DELETE" });
    },
    [send],
  );

  // Expose a fixed same-origin operation, not an arbitrary authenticated URL.
  const approveDevice = useCallback(
    (userCode: string): Promise<{ message?: string }> =>
      postJson<{ message?: string }>("/auth/device/approve", { user_code: userCode }),
    [postJson],
  );

  return useMemo(
    () => ({ get, post, del, approveDevice }),
    [get, post, del, approveDevice],
  );
}
