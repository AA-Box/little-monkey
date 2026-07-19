/** Tiny display formatters shared by the chat status line, the grouped
 * subagents card, and the Background-tasks drawer — kept here (rather than
 * per-component copies) because all three must render the same value the
 * same way for the "one task, three surfaces" UI to read as one system. */

/** `92_000` → `"1m 32s"`, `4_000` → `"4s"`, `3_720_000` → `"1h 2m"`. */
export function formatElapsed(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  if (totalSeconds < 60) return `${totalSeconds}s`;
  const totalMinutes = Math.floor(totalSeconds / 60);
  if (totalMinutes < 60) return `${totalMinutes}m ${totalSeconds % 60}s`;
  return `${Math.floor(totalMinutes / 60)}h ${totalMinutes % 60}m`;
}

/** `181_142` → `"181.1k"`, `950` → `"950"` — the compact token count next
 * to a task's elapsed time. Keeps the trailing `.0` (`"188.0k"`) so the
 * label width stays stable while a live count ticks up. */
export function formatCompactTokens(count: number): string {
  if (count < 1000) return `${count}`;
  return `${(count / 1000).toFixed(1)}k`;
}
