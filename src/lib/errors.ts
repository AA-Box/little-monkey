/**
 * The single way this codebase turns an unknown caught value into display
 * text.
 *
 * `catch` binds `unknown`, so every call site previously re-rolled
 * `error instanceof Error ? error.message : String(error)` — 300+ copies of
 * the same three-way decision, which meant any improvement (Tauri's string
 * rejections, nested causes, non-Error objects) had to be made 300+ times or
 * not at all.
 *
 * Behavior is deliberately a superset of the pattern it replaces: an `Error`
 * still yields exactly `error.message`, and anything else still falls back to
 * `String(value)`. The extra cases below only fire where `String(value)`
 * would have produced something useless like `"[object Object]"`.
 */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (error !== null && typeof error === "object") {
    // Tauri IPC rejections and structured-clone'd errors arrive as plain
    // objects that still carry a usable message/error field. Without this,
    // `String(value)` renders "[object Object]" and the real reason is lost.
    const record = error as Record<string, unknown>;
    for (const key of ["message", "error", "reason"] as const) {
      const value = record[key];
      if (typeof value === "string" && value.length > 0) return value;
    }
    try {
      const serialized = JSON.stringify(error);
      if (serialized && serialized !== "{}") return serialized;
    } catch {
      // Circular or non-serializable — fall through to String() below.
    }
  }
  return String(error);
}
