import { sessionDisplayTitle, type ChatSession, type SessionGroup } from "../../store/sessionStore";
import {
  BUILT_IN_ENVIRONMENTS,
  LOCAL_ENVIRONMENT,
  type ExternalConversation,
} from "../../lib/conversationsClient";
import type { SessionStatus } from "./sessionStatus";

/**
 * How the sidebar's session list is filtered, grouped and ordered.
 *
 * The list holds sessions from more than one environment: this desktop's own
 * (`local`), a paired phone's chat (`remote_control`), and a messaging
 * conversation the agent is answering (`channel:<provider>`). They are
 * different things underneath — one is a transcript this app owns, the others
 * are conversations the daemon owns — so this module normalizes both into one
 * row type, and everything downstream (filtering, grouping, sorting) works on
 * rows rather than on either source.
 *
 * The four axes:
 *
 * - **Status** — active / archived / all. Only local sessions can be
 *   archived, so an outside conversation is always "active".
 * - **Environment** — a multi-select over the environments above. The
 *   built-in three are always offered even when nothing has arrived on one:
 *   an empty Slack filter says "no Slack workspace has installed this yet",
 *   which is worth being able to see.
 * - **Group by** — date, workspace folder, run state, the user's own custom
 *   groups, or one flat list.
 * - **Sort by** — alphabetically, creation time, or most recent activity.
 *
 * Pinned sessions sit outside grouping (a pin is an explicit "keep this at the
 * top") but inside sorting and filtering.
 */

export type StatusFilter = "active" | "archived" | "all";
export type GroupBy = "date" | "folder" | "state" | "groups" | "none";
export type SortBy = "alphabetical" | "created" | "recency";

export interface SessionListPrefs {
  status: StatusFilter;
  /** Selected environments. Empty means every environment — the same thing
   * the menu's "All environments" checkbox says, kept as emptiness rather
   * than an enumeration so an environment that appears later is included
   * without the stored preference having to know about it. */
  environments: string[];
  groupBy: GroupBy;
  sortBy: SortBy;
}

export const DEFAULT_SESSION_LIST_PREFS: SessionListPrefs = {
  status: "active",
  environments: [],
  groupBy: "groups",
  sortBy: "recency",
};

/** One row of the list, whichever environment it came from. */
export type SessionRow =
  | {
      kind: "local";
      id: string;
      environment: typeof LOCAL_ENVIRONMENT;
      title: string;
      createdAt: number;
      updatedAt: number;
      pinned: boolean;
      archived: boolean;
      groupId: string | null;
      workspacePath: string | null;
      status: SessionStatus | null;
      session: ChatSession;
    }
  | {
      kind: "external";
      id: string;
      environment: string;
      title: string;
      createdAt: number;
      updatedAt: number;
      pinned: false;
      archived: false;
      groupId: null;
      workspacePath: null;
      status: null;
      conversation: ExternalConversation;
    };

/** Labels the sections need, passed in so this module stays i18n-free. */
export interface SectionLabels {
  recents: string;
  today: string;
  yesterday: string;
  lastWeek: string;
  older: string;
  noFolder: string;
  /** Bucket for rows with no run state of their own — every outside
   * conversation, and any local session that has simply been quiet. */
  idle: string;
  /** Per-status bucket titles, keyed exactly like `SessionStatus`. */
  state: Record<SessionStatus, string>;
}

export interface SessionListSection {
  /** Stable React key: a group id, a bucket name, or "recents". */
  id: string;
  title: string;
  items: SessionRow[];
}

export interface SessionListView {
  pinned: SessionRow[];
  sections: SessionListSection[];
  archived: SessionRow[];
  /** True when a filter (not an empty account) is why nothing is listed. */
  filtered: boolean;
}

export function localRow(session: ChatSession, status: SessionStatus | null): SessionRow {
  return {
    kind: "local",
    id: session.id,
    environment: LOCAL_ENVIRONMENT,
    title: sessionDisplayTitle(session),
    createdAt: session.createdAt,
    updatedAt: session.updatedAt,
    pinned: session.pinned,
    archived: session.archived,
    groupId: session.groupId,
    workspacePath: session.workspacePath,
    status,
    session,
  };
}

export function externalRow(conversation: ExternalConversation): SessionRow {
  return {
    kind: "external",
    id: `${conversation.environment} ${conversation.id}`,
    environment: conversation.environment,
    // A phone's chat is titled by its first message and a channel thread by
    // whatever the provider called it; both can be empty, and an untitled row
    // is worse than one named after the account it arrived on.
    title: conversation.title.trim() || conversation.account_label?.trim() || conversation.id,
    // The daemon records activity, not creation. Sorting by "created time"
    // therefore orders these by the only timestamp there is — honest, and
    // stable, rather than a fabricated birthday.
    createdAt: conversation.updated_at_ms,
    updatedAt: conversation.updated_at_ms,
    pinned: false,
    archived: false,
    groupId: null,
    workspacePath: null,
    status: null,
    conversation,
  };
}

/** Every environment the filter offers: the built-ins, then any other one
 * that actually has a conversation (a second messaging provider, say). */
export function environmentOptions(rows: readonly SessionRow[]): string[] {
  const builtIn: readonly string[] = BUILT_IN_ENVIRONMENTS;
  const extra = new Set<string>();
  for (const row of rows) {
    if (!builtIn.includes(row.environment)) extra.add(row.environment);
  }
  return [...builtIn, ...[...extra].sort()];
}

export function matchesStatus(row: SessionRow, filter: StatusFilter): boolean {
  switch (filter) {
    case "all":
      return true;
    case "active":
      return !row.archived;
    case "archived":
      return row.archived;
  }
}

export function matchesEnvironment(row: SessionRow, environments: readonly string[]): boolean {
  return environments.length === 0 || environments.includes(row.environment);
}

export function comparatorFor(sortBy: SortBy): (a: SessionRow, b: SessionRow) => number {
  switch (sortBy) {
    case "recency":
      return (a, b) => {
        // Keep active work together at the top, but never compare two
        // working rows by updatedAt: streaming patches update that field and
        // would otherwise make the rows swap places continuously. Returning
        // 0 preserves their existing order through the stable sort.
        if (a.status === "working" && b.status !== "working") return -1;
        if (a.status !== "working" && b.status === "working") return 1;
        if (a.status === "working" && b.status === "working") return 0;
        return b.updatedAt - a.updatedAt;
      };
    case "created":
      return (a, b) => b.createdAt - a.createdAt;
    case "alphabetical":
      return (a, b) => a.title.localeCompare(b.title);
  }
}

const DAY_MS = 86_400_000;
const DATE_BUCKETS = ["today", "yesterday", "lastWeek", "older"] as const;
type DateBucket = (typeof DATE_BUCKETS)[number];

/** Which calendar bucket a timestamp falls in, relative to `now`'s own day. */
function dateBucket(at: number, now: number): DateBucket {
  const startOfToday = new Date(now).setHours(0, 0, 0, 0);
  if (at >= startOfToday) return "today";
  if (at >= startOfToday - DAY_MS) return "yesterday";
  if (at >= startOfToday - 7 * DAY_MS) return "lastWeek";
  return "older";
}

/** The workspace a row belongs to, as a display label. */
function folderLabel(row: SessionRow, fallback: string): string {
  if (!row.workspacePath) return fallback;
  return row.workspacePath.split(/[\\/]/).filter(Boolean).pop() ?? row.workspacePath;
}

/** Buckets rows in the order the keys first appear, so a grouping keeps
 * whatever order the sort already put the rows in rather than inventing one. */
function bucketBy(
  rows: readonly SessionRow[],
  keyOf: (row: SessionRow) => { id: string; title: string },
): SessionListSection[] {
  const sections = new Map<string, SessionListSection>();
  for (const row of rows) {
    const { id, title } = keyOf(row);
    const section = sections.get(id);
    if (section) section.items.push(row);
    else sections.set(id, { id, title, items: [row] });
  }
  return [...sections.values()];
}

/**
 * Splits `rows` into what the list renders. `rows` is every session worth
 * listing — local ones already filtered to "has messages", plus every outside
 * conversation.
 */
export function buildSessionListView({
  rows,
  groups,
  prefs,
  now,
  labels,
}: {
  rows: readonly SessionRow[];
  groups: readonly SessionGroup[];
  prefs: SessionListPrefs;
  now: number;
  labels: SectionLabels;
}): SessionListView {
  const compare = comparatorFor(prefs.sortBy);
  const visible = rows.filter(
    (row) => matchesStatus(row, prefs.status) && matchesEnvironment(row, prefs.environments),
  );
  const pinned = visible.filter((row) => row.pinned).sort(compare);
  const rest = visible.filter((row) => !row.pinned).sort(compare);
  // Only the default view keeps archived sessions behind the collapsed
  // footer: asking for "Archived" means they are the list, not a footnote to
  // it, and "All" already has them in the sections above.
  const archived =
    prefs.status === "active"
      ? rows
          .filter((row) => row.archived && matchesEnvironment(row, prefs.environments))
          .sort(compare)
      : [];

  let sections: SessionListSection[];
  switch (prefs.groupBy) {
    case "none":
      sections = [{ id: "recents", title: labels.recents, items: [...rest] }];
      break;
    case "date":
      // Cut on the same timestamp the "recency" sort reads, so "Today" always
      // means "was active today" whatever the sort is.
      sections = DATE_BUCKETS.map((bucket) => ({
        id: bucket,
        title: labels[bucket],
        items: rest.filter((row) => dateBucket(row.updatedAt, now) === bucket),
      })).filter((section) => section.items.length > 0);
      break;
    case "folder":
      sections = bucketBy(rest, (row) => ({
        id: `folder:${row.workspacePath ?? ""}`,
        title: folderLabel(row, labels.noFolder),
      }));
      break;
    case "state":
      sections = bucketBy(rest, (row) => ({
        id: `state:${row.status ?? "idle"}`,
        title: row.status ? labels.state[row.status] : labels.idle,
      }));
      break;
    case "groups":
      sections = groups
        .map((group) => ({
          id: group.id,
          title: group.name,
          items: rest.filter((row) => row.groupId === group.id),
        }))
        .filter((section) => section.items.length > 0);
      // Everything with no group of its own — including every outside
      // conversation — keeps the "Recents" section, even when empty: it is
      // the list's own heading, and the row the view menu hangs off.
      sections.push({
        id: "recents",
        title: labels.recents,
        items: rest.filter((row) => !row.groupId),
      });
      break;
  }

  const filtered =
    prefs.status !== DEFAULT_SESSION_LIST_PREFS.status || prefs.environments.length > 0;
  return { pinned, sections, archived, filtered };
}
