import { Component, type ErrorInfo, type ReactNode } from "react";

import { Button } from "./ui";
import { useT } from "../lib/i18n";

function formatError(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

/** Separate function component so the fallback can use hooks (`useT`) — the
 * boundary itself must be a class, and locale lives in a zustand store that
 * stays readable even when the subtree under the boundary has crashed. */
function ErrorFallback({ error }: { error: unknown }) {
  const { t } = useT();

  return (
    <div
      role="alert"
      className="flex h-full min-h-0 w-full flex-1 flex-col items-center justify-center gap-3 bg-background p-8 text-center"
    >
      <h2 className="text-base font-semibold text-foreground">{t("ErrorBoundary.title")}</h2>
      <p className="max-w-md text-sm text-muted">{t("ErrorBoundary.description")}</p>
      <pre className="max-h-40 max-w-lg overflow-auto whitespace-pre-wrap break-words rounded-md border border-border bg-surface px-3 py-2 text-left text-xs text-danger">
        {formatError(error)}
      </pre>
      <Button variant="primary" onClick={() => window.location.reload()}>
        {t("ErrorBoundary.reload")}
      </Button>
    </div>
  );
}

interface ErrorBoundaryProps {
  children: ReactNode;
  /** When this changes while an error is shown, the error is cleared and the
   * children get a fresh render — lets a crashed ChatWindow pane recover on
   * session switch without remounting (and losing state in) healthy panes. */
  resetKey?: unknown;
}

interface ErrorBoundaryState {
  hasError: boolean;
  error: unknown;
  lastResetKey: unknown;
}

/** Class component because React only exposes error-catching lifecycles
 * (`getDerivedStateFromError`/`componentDidCatch`) on classes — there is no
 * hook equivalent. Rendering and copy live in `ErrorFallback` above. */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { hasError: false, error: null, lastResetKey: this.props.resetKey };

  static getDerivedStateFromError(error: unknown): Partial<ErrorBoundaryState> {
    return { hasError: true, error };
  }

  static getDerivedStateFromProps(
    props: ErrorBoundaryProps,
    state: ErrorBoundaryState,
  ): Partial<ErrorBoundaryState> | null {
    if (props.resetKey !== state.lastResetKey) {
      return { lastResetKey: props.resetKey, hasError: false, error: null };
    }
    return null;
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error("ErrorBoundary caught a render error:", error, info.componentStack);
  }

  render() {
    if (this.state.hasError) return <ErrorFallback error={this.state.error} />;
    return this.props.children;
  }
}

export default ErrorBoundary;
