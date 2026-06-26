import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";

export function AdminLogin() {
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const navigate = useNavigate();

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError("");
    setLoading(true);
    try {
      const res = await fetch("/admin/api/status", {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (res.status === 401) {
        setError("Invalid admin token");
        return;
      }
      if (!res.ok) {
        setError(`Server error: ${res.status}`);
        return;
      }
      sessionStorage.setItem("admin_token", token);
      navigate("/admin", { replace: true });
    } catch {
      setError("Could not connect to the server");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="flex h-full items-center justify-center bg-[var(--color-surface)]">
      <form
        onSubmit={handleSubmit}
        className="w-full max-w-sm rounded-xl border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-8 shadow-lg"
      >
        <h2 className="mb-6 text-center text-xl font-semibold text-[var(--color-text)]">
          NestWeaver Admin
        </h2>
        <label className="mb-2 block text-sm text-[var(--color-text-muted)]">
          Admin Token
        </label>
        <input
          type="password"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="nw_admin_..."
          autoFocus
          className="mb-4 w-full rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-2 text-sm text-[var(--color-text)] outline-none focus:border-[var(--color-graph-selection)]"
        />
        {error && (
          <p className="mb-4 text-sm text-red-500">{error}</p>
        )}
        <button
          type="submit"
          disabled={loading || !token}
          className="w-full rounded-md bg-[var(--color-graph-selection)] px-4 py-2 text-sm font-medium text-white transition-opacity disabled:opacity-50"
        >
          {loading ? "Verifying..." : "Sign In"}
        </button>
      </form>
    </div>
  );
}
