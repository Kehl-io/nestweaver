import { useState, useEffect, type FormEvent } from "react";
import { useSearchParams } from "react-router-dom";

export function DeviceApprove() {
  const [searchParams] = useSearchParams();
  const [userCode, setUserCode] = useState(searchParams.get("user_code") ?? "");
  const [status, setStatus] = useState<"idle" | "loading" | "success" | "error">("idle");
  const [message, setMessage] = useState("");

  // Pre-fill from query param if present.
  useEffect(() => {
    const code = searchParams.get("user_code");
    if (code) setUserCode(code);
  }, [searchParams]);

  async function handleApprove(e: FormEvent) {
    e.preventDefault();
    if (!userCode.trim()) return;
    setStatus("loading");
    setMessage("");
    try {
      const res = await fetch("/auth/device/approve", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ user_code: userCode.trim() }),
      });
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      const data: { message?: string } = await res.json();
      setStatus("success");
      setMessage(data.message ?? "Device approved successfully.");
    } catch (err: unknown) {
      setStatus("error");
      setMessage(err instanceof Error ? err.message : "Approval failed");
    }
  }

  return (
    <div className="mx-auto max-w-md space-y-6">
      <h2 className="text-lg font-semibold text-[var(--color-text)]">
        Approve Device
      </h2>
      <p className="text-sm text-[var(--color-text-muted)]">
        A developer is requesting access via the device authorization flow.
        Enter the user code displayed on their device to grant access.
      </p>

      {status === "success" ? (
        <div className="rounded-lg border border-green-300 bg-green-50 p-4 text-green-700 dark:border-green-800 dark:bg-green-900/20 dark:text-green-400">
          {message}
        </div>
      ) : (
        <form onSubmit={handleApprove} className="space-y-4">
          <div>
            <label className="mb-1 block text-sm text-[var(--color-text-muted)]">
              User Code
            </label>
            <input
              type="text"
              value={userCode}
              onChange={(e) => setUserCode(e.target.value)}
              placeholder="e.g. ABCD1234"
              autoFocus
              className="w-full rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-2 text-center text-lg font-mono tracking-widest text-[var(--color-text)] outline-none focus:border-[var(--color-graph-selection)]"
            />
          </div>

          {status === "error" && (
            <div className="rounded-lg border border-red-300 bg-red-50 p-4 text-sm text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
              {message}
            </div>
          )}

          <button
            type="submit"
            disabled={status === "loading" || !userCode.trim()}
            className="w-full rounded-md bg-[var(--color-graph-selection)] px-4 py-2 text-sm font-medium text-white transition-opacity disabled:opacity-50"
          >
            {status === "loading" ? "Approving..." : "Approve Device"}
          </button>
        </form>
      )}
    </div>
  );
}
