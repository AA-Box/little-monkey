import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "portability-test" }) }));

import { invoke } from "@tauri-apps/api/core";
import type { ModelTargetSnapshot } from "./modelTargets";
import {
  buildPortableBundleRequest,
  contentBlocks,
  importPortableOutcome,
  recoverPendingPortableSettings,
  runWebDavBackupDue,
  sanitizePortableMetadata,
  stageEncryptedSnapshot,
  type PortableReadOutcome,
} from "./portability";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { usePromptStore } from "../store/promptStore";
import { useStackStore } from "../store/stackStore";
import { LOCALE_STORAGE_KEY, useLocaleStore } from "../store/localeStore";
import { SHORTCUT_STORAGE_KEY, useShortcutStore } from "../store/shortcutStore";

const TARGET: ModelTargetSnapshot = {
  kind: "provider",
  key: "provider:test:model",
  label: "Test",
  displayName: "model",
  providerId: "test",
  endpoint: "https://provider.test/v1",
  model: "model",
  credentialRefId: "keychain:com.littlemonkey.app:test",
  capabilities: {
    toolCalling: { state: "unknown", evidence: "fixture" },
    vision: { state: "unknown", evidence: "fixture" },
  },
  availability: { status: "available", evidence: "fixture" },
};

function source(): ChatSession {
  return {
    id: "session-stable",
    title: "Portable fixture",
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: "Intro\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n\n```ts\nconst n = 1;\n```" },
          { type: "image_url", image_url: { url: "data:image/png;base64,AQIDBA==" } },
        ],
      },
      { role: "assistant", content: "Done" },
    ],
    createdAt: 100,
    updatedAt: 200,
    pinned: true,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: TARGET,
    comparisonBranch: null,
    crewRun: null,
    workspacePath: "/tmp/project",
    personaId: "persona-a",
    attachedStackIds: ["stack-a"],
    docChatMode: true,
    subagentRuns: {},
    messageTranslations: [{
      messageIndex: 1,
      role: "assistant",
      locale: "es",
      originalContent: "Done",
      translatedText: "Hecho",
      sourceSha256: "0".repeat(64),
      createdAt: 300,
      modelTarget: TARGET,
    }],
    threadTranslations: [{
      locale: "es",
      originalTitle: "Portable fixture",
      translatedTitle: "Ejemplo portátil",
      sourceSha256: "1".repeat(64),
      translatedMessageIndices: [1],
      createdAt: 301,
      modelTarget: TARGET,
    }],
    displayTranslationLocale: "es",
  };
}

function seed(session = source()): void {
  useSessionStore.setState({
    sessions: [session],
    activeSessionId: session.id,
    messages: session.messages,
    groups: [],
    crews: [],
    runningTurns: {},
    runningSyntheses: {},
    runningCrews: {},
    runningVerifyLabel: {},
    persistError: null,
  });
  usePromptStore.setState({ entries: [], defaultPersonaId: null, hasSeededDefaults: true, persistError: null });
  useStackStore.setState({ stacks: [] });
}

beforeEach(() => {
  const storage = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => { storage.set(key, value); },
    removeItem: (key: string) => { storage.delete(key); },
  });
  localStorage.removeItem(LOCALE_STORAGE_KEY);
  localStorage.removeItem(SHORTCUT_STORAGE_KEY);
  useLocaleStore.setState({ locale: "en-US" });
  useShortcutStore.setState({ overrides: {} });
  seed();
  vi.mocked(invoke).mockReset();
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "portable_restore_apply") {
      const request = (args as { request: { stacks: unknown[] } }).request;
      return {
        transactionId: "11111111-1111-4111-8111-111111111111",
        stacks: request.stacks,
        profileCounts: {
          groups: 0,
          sessions: 1,
          messages: 2,
          actorTranscripts: 0,
          crews: 0,
          attachmentOccurrences: 1,
          uniqueArtifacts: 1,
        },
        settingsPending: true,
      } as never;
    }
    if (command === "portable_restore_settings_acknowledge") return true as never;
    return null as never;
  });
});

describe("portable profile conversion", () => {
  it("stages the exact frontend bundle and delegates due execution to the shared scheduler", async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce({
        path: "/private/backups/daemon-staged.lmsnapshot",
        createdAtMs: 123,
        byteSize: 456,
        sha256: "a".repeat(64),
        sourceRevisionSha256: "b".repeat(64),
      } as never)
      .mockResolvedValueOnce({ status: "already_current", snapshotSha256: "a".repeat(64), nextDueMs: 999 } as never);

    await expect(stageEncryptedSnapshot()).resolves.toMatchObject({ createdAtMs: 123, byteSize: 456 });
    await expect(runWebDavBackupDue(true)).resolves.toMatchObject({ status: "already_current" });
    expect(vi.mocked(invoke).mock.calls[0][0]).toBe("portable_snapshot_stage_source");
    expect(vi.mocked(invoke).mock.calls[0][1]).toMatchObject({
      request: { data: { sessions: [{ id: "session-stable" }] } },
    });
    expect(vi.mocked(invoke).mock.calls[1]).toEqual(["portable_webdav_run_due", { force: true }]);
  });

  it("preserves text/code/table order and strips credential schema fields", () => {
    expect(contentBlocks("before\n```js\ncode();\n```\nafter").map((block) => block.type)).toEqual([
      "text",
      "code",
      "text",
    ]);
    expect(contentBlocks("| A | B |\n| --- | --- |\n| 1 | 2 |")[0]).toMatchObject({ type: "table", headers: ["A", "B"] });
    expect(sanitizePortableMetadata({ nested: { apiKey: "nope", safe: 1 }, passwordHint: "nope" })).toEqual({
      nested: { safe: 1 },
    });
  });

  it("builds stable ids and byte-identical attachment artifacts, then restores originals and translations", async () => {
    const request = await buildPortableBundleRequest();
    const portable = request.data.sessions[0];
    expect(portable.id).toBe("session-stable");
    expect(portable.messages.map((message) => message.id)).toEqual([
      "message-session-stable-0",
      "message-session-stable-1",
    ]);
    expect(request.artifacts).toHaveLength(1);
    expect(request.artifacts[0].bytesBase64).toBe("AQIDBA==");
    expect(JSON.stringify(request)).not.toContain("credentialRefId");

    const outcome: PortableReadOutcome = {
      data: request.data,
      artifacts: request.artifacts.map((artifact) => ({
        id: artifact.id,
        mediaType: artifact.mediaType,
        bytesBase64: artifact.bytesBase64,
      })),
      preflight: {
        archiveSha256: "a".repeat(64),
        entryCount: 3,
        compressedBytes: 1,
        expandedBytes: 1,
        sessionCount: 1,
        messageCount: 2,
        artifactCount: 1,
        externalReferenceCount: 0,
      },
    };
    seed({ ...source(), id: "throw-away", messages: [] });
    await expect(importPortableOutcome(outcome, "replace")).resolves.toBe(1);
    const restored = useSessionStore.getState().sessions[0];
    expect(restored.id).toBe("session-stable");
    expect(JSON.stringify(restored.messages[0].content)).toContain("data:image/png;base64,AQIDBA==");
    expect(restored.modelTarget).toMatchObject({ providerId: "test", credentialRefId: "keychain:com.littlemonkey.app:test" });
    expect(restored.messageTranslations?.[0]).toMatchObject({ locale: "es", translatedText: "Hecho" });
    expect(restored.threadTranslations?.[0]).toMatchObject({ translatedTitle: "Ejemplo portátil" });
    expect(restored.displayTranslationLocale).toBe("es");
    expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "portable_restore_apply")).toHaveLength(1);
    expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "stacks_import_definitions")).toBe(false);
  });

  it("does not mutate any frontend store when the atomic backend restore rejects", async () => {
    const request = await buildPortableBundleRequest();
    const outcome: PortableReadOutcome = {
      data: request.data,
      artifacts: request.artifacts.map(({ id, mediaType, bytesBase64 }) => ({ id, mediaType, bytesBase64 })),
      preflight: {
        archiveSha256: "b".repeat(64),
        entryCount: 3,
        compressedBytes: 1,
        expandedBytes: 1,
        sessionCount: 1,
        messageCount: 2,
        artifactCount: 1,
        externalReferenceCount: 0,
      },
    };
    const localSession = { ...source(), id: "local-session", title: "Must survive" };
    seed(localSession);
    usePromptStore.setState({
      entries: [{
        id: "local-prompt",
        kind: "snippet",
        name: "Local",
        command: "local",
        content: "keep",
        createdAt: 1,
        updatedAt: 1,
      }],
      defaultPersonaId: null,
      hasSeededDefaults: true,
    });
    useStackStore.setState({ stacks: [{
      id: "local-stack",
      name: "Local stack",
      sources: [],
      embedding: { backend: "ollama", model_id_or_tag: "embed", dim: 3, query_prefix: "", doc_prefix: "" },
      chunk_chars: 100,
      chunk_overlap: 10,
      indexed_at: null,
      chunk_count: 0,
    }] });
    vi.mocked(invoke).mockRejectedValueOnce(new Error("injected transaction failure"));

    await expect(importPortableOutcome(outcome, "replace")).rejects.toThrow("injected transaction failure");
    expect(useSessionStore.getState().sessions).toEqual([localSession]);
    expect(usePromptStore.getState().entries.map((entry) => entry.id)).toEqual(["local-prompt"]);
    expect(useStackStore.getState().stacks.map((stack) => stack.id)).toEqual(["local-stack"]);
  });

  it("replays and acknowledges crash-pending settings only after browser persistence succeeds", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "portable_restore_settings_pending") {
        return {
          schemaVersion: 1,
          transactionId: "22222222-2222-4222-8222-222222222222",
          locale: "fr-FR",
          shortcutOverrides: {},
        } as never;
      }
      if (command === "portable_restore_settings_acknowledge") return true as never;
      return null as never;
    });

    await expect(recoverPendingPortableSettings()).resolves.toBe(true);
    expect(useLocaleStore.getState().locale).toBe("fr-FR");
    expect(useShortcutStore.getState().overrides).toEqual({});
    expect(localStorage.getItem(LOCALE_STORAGE_KEY)).toBe("fr-FR");
    expect(JSON.parse(localStorage.getItem(SHORTCUT_STORAGE_KEY) ?? "null")).toEqual({
      version: 1,
      overrides: {},
    });
    expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual([
      "portable_restore_settings_pending",
      "portable_restore_settings_acknowledge",
    ]);
  });
});
