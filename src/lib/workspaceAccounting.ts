/**
 * The K25 join: what a workspace cost in tokens, and what it cost the machine.
 *
 * Two ledgers already existed and never met. `costControlStore.ts` records a
 * priced provider call per session/run; the K6 process ledger
 * (`process_usage_ledger`) records what each process actually consumed. Both
 * now carry the same workspace identity — a filesystem path — so a workspace's
 * device time is accountable next to its token bill instead of only its token
 * bill being accountable at all.
 *
 * Two rules carry over unchanged from the surfaces being joined:
 *
 * - **`null` means unavailable, never zero.** {@link foldUsageRows} mirrors the
 *   Rust fold (`process_table.rs::fold_total`) exactly, including which fields
 *   are summed and which are maxima, and including that a total over rows that
 *   all left a field unmeasured stays `null`.
 * - **Unattributed is its own bucket.** A cost entry with no workspace and a
 *   process row with no workspace both land under the empty key rather than
 *   being dropped, because a per-workspace table that silently omits what it
 *   could not place reads as a complete accounting of the machine.
 *
 * The join is an exact path match. A process whose workspace is a subdirectory
 * of a chat's workspace root stays its own row: guessing that one path is
 * "inside" another is a claim about the disk this module cannot check.
 */
import {
  USAGE_FIELDS,
  type ProcessUsageAggregate,
  type ProcessUsageRow,
  type ProcessUsageTotal,
  type UsageTotalField,
} from "./processUsage";
import {
  attributeCost,
  type CostAttributionRow,
  type CostAttributionScope,
  type CostUsageEntry,
} from "../store/costControlStore";

function emptyTotal(): ProcessUsageTotal {
  return { value: null, measuredRows: 0, unavailableRows: 0 };
}

function emptyAggregate(): ProcessUsageAggregate {
  const aggregate = { rows: 0 } as ProcessUsageAggregate;
  for (const spec of USAGE_FIELDS) aggregate[spec.field] = emptyTotal();
  return aggregate;
}

function valueOf(row: ProcessUsageRow, field: UsageTotalField): number | null {
  if (field === "wallTimeMs") return row.wallTimeMs;
  const measured = row.usage as unknown as Record<string, number | null | undefined>;
  return measured[field] ?? null;
}

/**
 * Folds ledger rows the way the backend folds them — sums for consumption,
 * maxima for footprints.
 *
 * Client-side rather than one backend call per workspace: the panel already
 * holds every row it is grouping, and `process_usage_ledger` bounds its read,
 * so re-querying per workspace would issue N reads over a subset of rows this
 * process has in hand — and each read's own bound would silently reshape the
 * grouping.
 */
export function foldUsageRows(rows: readonly ProcessUsageRow[]): ProcessUsageAggregate {
  const aggregate = emptyAggregate();
  aggregate.rows = rows.length;
  for (const row of rows) {
    for (const spec of USAGE_FIELDS) {
      const total = aggregate[spec.field];
      const value = valueOf(row, spec.field);
      if (value === null) {
        total.unavailableRows += 1;
        continue;
      }
      total.measuredRows += 1;
      total.value =
        total.value === null
          ? value
          : spec.fold === "max"
            ? Math.max(total.value, value)
            : total.value + value;
    }
  }
  return aggregate;
}

/** One workspace's two bills, either of which may be absent. */
export interface WorkspaceAccountRow {
  /** The workspace path. Empty string is the unattributed bucket. */
  key: string;
  /** Recorded provider spend, or null when this workspace has no priced calls
   * recorded at all — distinct from a workspace whose calls summed to $0. */
  cost: CostAttributionRow | null;
  /** Measured device usage, or null when no process row named this workspace. */
  device: ProcessUsageAggregate | null;
}

/**
 * Every workspace either ledger knows about, most-spent first, then the ones
 * with device time only, then the unattributed bucket last.
 *
 * The unattributed bucket sorts last regardless of size because it is not a
 * workspace — leaving it interleaved would invite reading it as one.
 */
export function accountByWorkspace(
  entries: readonly CostUsageEntry[],
  ledgerRows: readonly ProcessUsageRow[],
  scope: CostAttributionScope = "workspace",
  fromMs = 0,
  nowMs = Date.now(),
): WorkspaceAccountRow[] {
  const costRows = attributeCost(entries, scope, fromMs, nowMs);
  const byWorkspace = new Map<string, ProcessUsageRow[]>();
  // Device rows only ever carry a workspace path, so they can only be joined
  // against the workspace scope. Grouping by project would put every process
  // under "unattributed", which reads as a claim that nothing was measured.
  if (scope === "workspace") {
    for (const row of ledgerRows) {
      const key = row.workspace ?? "";
      const held = byWorkspace.get(key);
      if (held) held.push(row);
      else byWorkspace.set(key, [row]);
    }
  }

  const keys = new Set<string>([
    ...costRows.map((row) => row.key),
    ...byWorkspace.keys(),
  ]);
  const rows: WorkspaceAccountRow[] = [];
  for (const key of keys) {
    const deviceRows = byWorkspace.get(key);
    rows.push({
      key,
      cost: costRows.find((row) => row.key === key) ?? null,
      device: deviceRows ? foldUsageRows(deviceRows) : null,
    });
  }

  return rows.sort((a, b) => {
    if (a.key === "" !== (b.key === "")) return a.key === "" ? 1 : -1;
    const bySpend = (b.cost?.spentUsd ?? 0) - (a.cost?.spentUsd ?? 0);
    if (bySpend !== 0) return bySpend;
    const byTokens = (b.cost?.totalTokens ?? 0) - (a.cost?.totalTokens ?? 0);
    if (byTokens !== 0) return byTokens;
    return (b.device?.rows ?? 0) - (a.device?.rows ?? 0);
  });
}

/** The trailing segment of a path, for labelling a row without losing which
 * folder it is — the full path stays available as the row's key. */
export function workspaceDisplayName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const separator = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return separator >= 0 ? trimmed.slice(separator + 1) || trimmed : trimmed;
}
