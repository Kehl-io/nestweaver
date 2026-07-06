import { NavLink, Outlet, Navigate } from "react-router-dom";

const navItems = [
  { to: "/admin", label: "Overview", end: true },
  { to: "/admin/repos", label: "Repos" },
  { to: "/admin/queue", label: "Queue" },
  { to: "/admin/dead-letter", label: "Dead Letter" },
  { to: "/admin/settings", label: "Settings" },
];

function AdminGuard({ children }: { children: React.ReactNode }) {
  const token = sessionStorage.getItem("admin_token");
  if (!token) return <Navigate to="/admin/login" replace />;
  return <>{children}</>;
}

export function AdminLayout() {
  return (
    <AdminGuard>
      <div className="flex h-full flex-col bg-[var(--color-surface)]">
        <header className="flex items-center gap-6 border-b border-[var(--color-border)] px-6 py-3">
          <h1 className="text-lg font-semibold text-[var(--color-text)]">
            NestWeaver Admin
          </h1>
          <nav className="flex gap-1">
            {navItems.map((item) => (
              <NavLink
                key={item.to}
                to={item.to}
                end={item.end}
                className={({ isActive }) =>
                  `rounded-md px-3 py-1.5 text-sm transition-colors ${
                    isActive
                      ? "bg-[var(--color-graph-selection)] text-white"
                      : "text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
                  }`
                }
              >
                {item.label}
              </NavLink>
            ))}
          </nav>
          <div className="ml-auto">
            <button
              onClick={() => {
                sessionStorage.removeItem("admin_token");
                window.location.href = "/admin/login";
              }}
              className="rounded-md px-3 py-1.5 text-sm text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
            >
              Logout
            </button>
          </div>
        </header>
        <main className="flex-1 overflow-auto p-6">
          <Outlet />
        </main>
      </div>
    </AdminGuard>
  );
}
