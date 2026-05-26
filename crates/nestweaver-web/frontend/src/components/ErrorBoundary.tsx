import { Component, type ReactNode } from "react";

interface Props { children: ReactNode; fallback?: ReactNode; }
interface State { hasError: boolean; error: Error | null; }

export class ErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false, error: null };

  static getDerivedStateFromError(error: Error) {
    return { hasError: true, error };
  }

  render() {
    if (this.state.hasError) {
      return this.props.fallback || (
        <div className="flex h-full items-center justify-center p-8 text-sm text-red-500">
          <div className="text-center">
            <p className="font-semibold mb-2">Something went wrong</p>
            <p className="text-xs text-[var(--color-text-muted)]">{this.state.error?.message}</p>
            <button onClick={() => this.setState({ hasError: false, error: null })}
              className="mt-3 px-3 py-1 rounded border border-[var(--color-border)] text-xs hover:bg-[var(--color-surface-alt)]">
              Try again
            </button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
