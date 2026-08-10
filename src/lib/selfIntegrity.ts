/**
 * The startup self-integrity verdict (roadmap K22), as the Updates panel reads
 * it. Mirrors `src-tauri/src/self_integrity.rs`.
 *
 * The verdict is computed once per process, so this is a read of a decision
 * that has already been made — including by every native launch path, which
 * refuses while `refused` is true.
 */
import { invoke, isTauri } from "@tauri-apps/api/core";

export type IntegrityStatus = "verified" | "mismatch" | "absent" | "unsupported" | "unverified";

export interface ComponentIntegrity {
  id: string;
  /** `signature` (the app bundle itself) or `runtime` (a managed component). */
  kind: string;
  status: IntegrityStatus;
  detail: string;
  path: string | null;
}

export interface IntegrityReport {
  checkedAtMs: number;
  /** True when something is present and provably wrong. While it is true, no
   * managed runtime binary can be launched. */
  refused: boolean;
  components: ComponentIntegrity[];
}

/** Null in a browser build, where there is no binary to verify. */
export async function loadIntegrityReport(): Promise<IntegrityReport | null> {
  if (!isTauri()) return null;
  return invoke<IntegrityReport>("self_integrity_report");
}

/** Sorted worst-first, so a refusal is the first thing on screen. */
export const STATUS_ORDER: Record<IntegrityStatus, number> = {
  mismatch: 0,
  unverified: 1,
  verified: 2,
  absent: 3,
  unsupported: 4,
};

export function sortComponents(components: ComponentIntegrity[]): ComponentIntegrity[] {
  return [...components].sort(
    (left, right) => STATUS_ORDER[left.status] - STATUS_ORDER[right.status],
  );
}
