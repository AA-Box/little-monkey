import { describe, expect, it } from "vitest";

import type { ChatSession, SessionGroup } from "../../store/sessionStore";
import type { ExternalConversation } from "../../lib/conversationsClient";
import {
  REMOTE_CONTROL_ENVIRONMENT,
  SLACK_ENVIRONMENT,
  LOCAL_ENVIRONMENT,
} from "../../lib/conversationsClient";
import type { SessionStatus } from "./sessionStatus";
import {
  DEFAULT_SESSION_LIST_PREFS,
  buildSessionListView,
  environmentOptions,
  externalRow,
  localRow,
  type SessionListPrefs,
  type SessionRow,
} from "./sessionListView";

const NOW = new Date("2026-08-16T15:00:00Z").getTime();
const DAY = 86_400_000;

function session(overrides: Partial<ChatSession> = {}): ChatSession {
  return {
    id: "s1",
    title: "session",
    messages: [],
    createdAt: NOW,
    updatedAt: NOW,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    ...overrides,
  };
}

function local(overrides: Partial<ChatSession> = {}, status: SessionStatus | null = null) {
  return localRow(session(overrides), status);
}

function external(overrides: Partial<ExternalConversation> = {}) {
  return externalRow({
    environment: REMOTE_CONTROL_ENVIRONMENT,
    provider: null,
    id: "phone-1",
    title: "From the phone",
    account_label: null,
    updated_at_ms: NOW,
    message_count: 2,
    ...overrides,
  });
}

const LABELS = {
  recents: "Recents",
  today: "Today",
  yesterday: "Yesterday",
  lastWeek: "Previous 7 days",
  older: "Older",
  noFolder: "No folder",
  idle: "Idle",
  state: {
    working: "Working",
    attention: "Waiting for you",
    error: "Failed",
    finished: "Finished",
  },
};

function view(
  rows: SessionRow[],
  prefs: Partial<SessionListPrefs> = {},
  groups: SessionGroup[] = [],
) {
  return buildSessionListView({
    rows,
    groups,
    prefs: { ...DEFAULT_SESSION_LIST_PREFS, ...prefs },
    now: NOW,
    labels: LABELS,
  });
}

const ids = (rows: readonly SessionRow[]) => rows.map((row) => row.id);
const flat = (sections: { items: SessionRow[] }[]) => ids(sections.flatMap((s) => s.items));

describe("buildSessionListView", () => {
  it("keeps today's default: pinned first, custom groups, then recents by recency", () => {
    const group: SessionGroup = { id: "g1", name: "Work", kind: "folder", createdAt: NOW };
    const rows = [
      local({ id: "pin", pinned: true, updatedAt: NOW - DAY }),
      local({ id: "in-group", groupId: "g1" }),
      local({ id: "older", updatedAt: NOW - 2 * DAY }),
      local({ id: "newer", updatedAt: NOW - 1000 }),
    ];

    const result = view(rows, {}, [group]);

    expect(ids(result.pinned)).toEqual(["pin"]);
    expect(result.sections.map((s) => s.title)).toEqual(["Work", "Recents"]);
    expect(ids(result.sections[0].items)).toEqual(["in-group"]);
    expect(ids(result.sections[1].items)).toEqual(["newer", "older"]);
    expect(result.filtered).toBe(false);
  });

  it("lists outside conversations beside local sessions, newest first", () => {
    const rows = [
      local({ id: "local-1", updatedAt: NOW - DAY }),
      external({ id: "phone-1", updated_at_ms: NOW }),
    ];

    expect(flat(view(rows).sections)).toEqual([
      `${REMOTE_CONTROL_ENVIRONMENT} phone-1`,
      "local-1",
    ]);
  });

  it("keeps working sessions together without reordering them during streaming", () => {
    const rows = [
      local({ id: "working-first", updatedAt: NOW - DAY }, "working"),
      local({ id: "working-second", updatedAt: NOW }, "working"),
      local({ id: "idle", updatedAt: NOW + DAY }),
    ];

    expect(flat(view(rows).sections)).toEqual(["working-first", "working-second", "idle"]);

    const afterStreaming = rows.map((row, index) => ({
      ...row,
      updatedAt: NOW + DAY + index,
    }));
    expect(flat(view(afterStreaming).sections)).toEqual(["working-first", "working-second", "idle"]);
  });

  it("offers the built-in environments even when nothing has arrived on one", () => {
    expect(environmentOptions([local()])).toEqual([
      LOCAL_ENVIRONMENT,
      REMOTE_CONTROL_ENVIRONMENT,
      SLACK_ENVIRONMENT,
    ]);
  });

  it("adds an environment that has conversations but is not built in", () => {
    const rows = [external({ environment: "channel:signal", provider: "signal", id: "c-1" })];

    expect(environmentOptions(rows)).toEqual([
      LOCAL_ENVIRONMENT,
      REMOTE_CONTROL_ENVIRONMENT,
      SLACK_ENVIRONMENT,
      "channel:signal",
    ]);
  });

  it("filters to the selected environments, and treats none selected as all", () => {
    const rows = [local({ id: "local-1" }), external({ id: "phone-1" })];

    expect(flat(view(rows, { environments: [REMOTE_CONTROL_ENVIRONMENT] }).sections)).toEqual([
      `${REMOTE_CONTROL_ENVIRONMENT} phone-1`,
    ]);
    expect(view(rows, { environments: [REMOTE_CONTROL_ENVIRONMENT] }).filtered).toBe(true);
    expect(flat(view(rows, { environments: [] }).sections)).toHaveLength(2);
    // An empty Slack filter is an answer, not a bug: no workspace has
    // installed the app yet.
    expect(flat(view(rows, { environments: [SLACK_ENVIRONMENT] }).sections)).toEqual([]);
  });

  it("hides archived sessions by default, shows only them on demand", () => {
    const rows = [
      local({ id: "live" }),
      local({ id: "old-archive", archived: true, updatedAt: NOW - DAY }),
      local({ id: "new-archive", archived: true }),
    ];

    const active = view(rows);
    expect(flat(active.sections)).toEqual(["live"]);
    expect(ids(active.archived)).toEqual(["new-archive", "old-archive"]);

    const archived = view(rows, { status: "archived" });
    expect(flat(archived.sections)).toEqual(["new-archive", "old-archive"]);
    // Nothing left over for the footer to duplicate.
    expect(archived.archived).toEqual([]);

    expect(flat(view(rows, { status: "all" }).sections)).toHaveLength(3);
  });

  it("buckets by calendar day, dropping empty buckets", () => {
    const rows = [
      local({ id: "today" }),
      local({ id: "yesterday", updatedAt: NOW - DAY }),
      local({ id: "ancient", updatedAt: NOW - 30 * DAY }),
    ];

    expect(
      view(rows, { groupBy: "date" }).sections.map((s) => [s.title, ids(s.items)]),
    ).toEqual([
      ["Today", ["today"]],
      ["Yesterday", ["yesterday"]],
      ["Older", ["ancient"]],
    ]);
  });

  it("buckets by folder, with outside conversations under 'No folder'", () => {
    const rows = [
      local({ id: "here", workspacePath: "/repos/app" }),
      local({ id: "nowhere" }),
      external({ id: "phone-1" }),
    ];

    expect(view(rows, { groupBy: "folder" }).sections.map((s) => [s.title, ids(s.items)])).toEqual([
      ["app", ["here"]],
      ["No folder", ["nowhere", `${REMOTE_CONTROL_ENVIRONMENT} phone-1`]],
    ]);
  });

  it("buckets by run state, with everything stateless under 'Idle'", () => {
    const rows = [
      local({ id: "busy" }, "working"),
      local({ id: "broken", updatedAt: NOW - 1 }, "error"),
      local({ id: "quiet", updatedAt: NOW - 2 }),
      external({ id: "phone-1", updated_at_ms: NOW - 3 }),
    ];

    expect(view(rows, { groupBy: "state" }).sections.map((s) => [s.title, ids(s.items)])).toEqual([
      ["Working", ["busy"]],
      ["Failed", ["broken"]],
      ["Idle", ["quiet", `${REMOTE_CONTROL_ENVIRONMENT} phone-1`]],
    ]);
  });

  it("sorts alphabetically and by creation time", () => {
    const rows = [
      local({ id: "b", title: "Beta", createdAt: NOW - DAY, updatedAt: NOW }),
      local({ id: "a", title: "Alpha", createdAt: NOW, updatedAt: NOW - DAY }),
    ];

    expect(flat(view(rows, { sortBy: "alphabetical" }).sections)).toEqual(["a", "b"]);
    expect(flat(view(rows, { sortBy: "created" }).sections)).toEqual(["a", "b"]);
    expect(flat(view(rows, { sortBy: "recency" }).sections)).toEqual(["b", "a"]);
  });

  it("titles an untitled outside conversation by its account, then its id", () => {
    expect(external({ title: "  ", account_label: "Ahmad's iPhone" }).title).toBe("Ahmad's iPhone");
    expect(external({ title: "", account_label: null, id: "phone-9" }).title).toBe("phone-9");
  });
});
