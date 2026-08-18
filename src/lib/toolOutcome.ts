import type { ToolOutcome } from "./runProtocol";

/**
 * How a tool result string classifies.
 *
 * One rule, in one place, because two callers need the same answer about the
 * same string and must never disagree: the durable ledger, which records what
 * a tool call did, and the learning loop's isolated evaluator, which reports
 * whether an arm's tool calls actually worked. A second copy of this would let
 * an evaluation call a failure a success.
 *
 * `executeToolCall` never throws — it returns a JSON `{ error }` payload — so
 * the failure signal is in the string rather than in an exception, and a
 * permission refusal is told apart from an ordinary failure by that error's
 * own wording.
 */
export function toolResultOutcome(result: string, cancelled: boolean): ToolOutcome {
  if (cancelled) return "cancelled";
  try {
    const parsed = JSON.parse(result) as { error?: unknown };
    if (parsed && typeof parsed === "object" && parsed.error) {
      return String(parsed.error).toLowerCase().includes("permission") ? "denied" : "failed";
    }
  } catch {
    // Successful plain-text tool results are expected.
  }
  return "succeeded";
}
