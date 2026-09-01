// @vitest-environment jsdom
/**
 * The route editor, driven the way an operator drives it.
 *
 * Two properties matter here and neither is cosmetic.
 *
 * A rung's ids are all mandatory. The daemon's sender rung is
 * `account + conversation + thread + sender`; a form that renders the thread
 * field but lets Save through without it submits a *different* scope than the
 * one the operator picked — silently a Conversation route, or a sender route
 * that would follow its sender into every thread. The daemon refuses both, so
 * the only question is whether the operator finds out here or after a
 * confusing error.
 *
 * And an edit must carry everything back. `update-route` replaces the target
 * wholesale, so a field this form forgets is a field the operator loses by
 * renaming a recipe.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));

import { ChannelRoutesSection, draftFrom, draftIncomplete, draftOptions } from "./ChannelRoutesSection";
import type { ChannelAccount, ChannelRoute } from "../../lib/channelsClient";
import { useModelStore } from "../../store/modelStore";
import type { RecipeTarget } from "../../store/recipeStore";

const ACCOUNT: ChannelAccount = {
  account_id: "chan-1",
  kind: "slack",
  label: "Team Slack",
  enabled: true,
  has_credential: true,
  credential_required: true,
  access_policy: { direct: "pairing", group: "allow_list", group_activation: "mention_only" },
  health: "connected",
  health_detail: null,
  last_error: null,
  last_probe_at_ms: 0,
  non_secret_config: {},
  created_at_ms: 0,
  callback_rejections: { count: 0, last_reason: null, last_at_ms: null },
  echo_correlation: "host_adapter",
  reply_policy_restricted: false,
  updated_at_ms: 0,
};

/** Every target field populated, so anything dropped in a round trip shows. */
const FULL_ROUTE: ChannelRoute = {
  route_id: "route-1",
  scope: {
    account_id: "chan-1",
    conversation_id: "C1",
    thread_id: "T1",
    sender_id: "U1",
  },
  target: {
    recipe: "triage",
    params: { focus: "deps", depth: "3" },
    repository: "/work/repo",
    session_scope: "sender",
    priority: 7,
    reply_to_conversation: false,
  },
  enabled: false,
  created_at_ms: 1,
  updated_at_ms: 1,
};

/** One saved task, as `recipes_list` reports it — the editor offers these
 * instead of a free-text name, so a test that picks one has to have it.
 *
 * The target matters as much as the name now: it is the model the route
 * answers on, and the row reads it from here exactly as the daemon reads it
 * from the file. */
function discovered(name: string, target: RecipeTarget = { ollama: "qwen2.5:7b" }) {
  return {
    path: `/tasks/${name}.yml`,
    source: "global",
    error: null,
    recipe: {
      version: 1,
      name,
      target,
      permission_mode: "manual",
      prompt: "{{message}}",
      params: { message: "" },
      output: { json: false },
    },
  };
}

/** The model inventory the picker is populated from, in the shape each
 * backend command really answers with. */
const INSTALLED_MODEL = {
  id: "Qwen2.5-7B",
  name: "Qwen2.5-7B",
  repo: "",
  file: "",
  size_gb: 4,
  tool_calling: true,
  installed: true,
  path: "/models/Qwen2.5-7B.gguf",
  is_external: false,
  kind: "chat",
};

const OLLAMA_MODEL = {
  name: "qwen3:8b",
  size_bytes: 1,
  is_cloud: false,
  tool_calling: true,
  vision: false,
  modified_at: "",
};

const OPENROUTER = {
  id: "openrouter",
  label: "OpenRouter",
  base_url: "https://openrouter.ai/api/v1",
  is_custom: false,
  has_key: true,
  is_extension: false,
};

interface Inventory {
  installed?: unknown[];
  ollamaReachable?: boolean;
  ollamaModels?: unknown[];
  providers?: unknown[];
  providerModels?: Record<string, unknown[]>;
}

const FULL_INVENTORY: Required<Inventory> = {
  installed: [INSTALLED_MODEL],
  ollamaReachable: true,
  ollamaModels: [OLLAMA_MODEL],
  providers: [OPENROUTER],
  providerModels: { openrouter: [{ id: "anthropic/claude-sonnet-4" }] },
};

function mockRoutes(
  routes: ChannelRoute[],
  recipes: (string | ReturnType<typeof discovered>)[] = ["chat", "triage", "triage-2"],
  inventory: Inventory = {},
  overrides: (command: string, args: unknown) => unknown = () => undefined,
) {
  const stock = { ...FULL_INVENTORY, ...inventory };
  invoke.mockImplementation((command: string, args: unknown) => {
    const custom = overrides(command, args);
    if (custom !== undefined) return custom;
    if (command === "channels_routes") return Promise.resolve({ routes });
    if (command === "channels_events") return Promise.resolve({ events: [] });
    if (command === "recipes_list")
      return Promise.resolve(
        recipes.map((entry) => (typeof entry === "string" ? discovered(entry) : entry)),
      );
    // The three inventories the model picker needs, through the same store
    // actions the Models, Ollama and provider panels use.
    if (command === "models_list_curated") return Promise.resolve([]);
    if (command === "models_list_installed") return Promise.resolve(stock.installed);
    if (command === "llama_status") return Promise.resolve({ status: "stopped" });
    if (command === "ollama_status")
      return Promise.resolve({
        reachable: stock.ollamaReachable,
        version: "0.1",
        binary_found: true,
      });
    if (command === "ollama_list_models") return Promise.resolve(stock.ollamaModels);
    if (command === "ollama_example_cloud_tags") return Promise.resolve([]);
    if (command === "providers_list_configured") return Promise.resolve(stock.providers);
    if (command === "providers_list_models") {
      const id = (args as { id: string }).id;
      return Promise.resolve(stock.providerModels[id] ?? []);
    }
    if (command === "providers_check_model_retirements") return Promise.resolve([]);
    return Promise.resolve(null);
  });
}

/** The one Model control on a single-route screen, once its inventory has
 * arrived. */
async function modelPicker(): Promise<HTMLSelectElement> {
  const picker = (await screen.findByLabelText("Model")) as HTMLSelectElement;
  // The inventory is fetched on mount; wait for it rather than asserting
  // against an empty list that is about to be replaced.
  await waitFor(() => expect(picker.querySelectorAll("optgroup").length).toBeGreaterThan(0));
  return picker;
}

afterEach(() => {
  cleanup();
  invoke.mockReset();
  // A module-level store outlives a test; a leftover inventory would make the
  // next one pass on data it never supplied.
  useModelStore.setState({
    installed: [],
    curated: [],
    ollamaModels: [],
    ollamaReachable: false,
    providers: [],
    providerModels: {},
  });
});

describe("the route draft", () => {
  it("is incomplete until every id its rung carries is named", () => {
    const base = {
      routeId: null,
      recipe: "chat",
      kind: "slack",
      accountId: "chan-1",
      conversationId: "C1",
      threadId: "",
      senderId: "",
      repository: "",
      params: [],
      sessionScope: "thread" as const,
      priority: "",
      reply: true,
      enabled: true,
    };

    expect(draftIncomplete({ ...base, level: "global" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "provider" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "account" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "conversation" })).toBe(false);
    // The two rungs that carry a thread require one.
    expect(draftIncomplete({ ...base, level: "thread" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "thread", threadId: "  " })).toBe(true);
    expect(draftIncomplete({ ...base, level: "thread", threadId: "T1" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "sender", senderId: "U1" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "sender", threadId: "T1" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "sender", threadId: "T1", senderId: "U1" })).toBe(false);
    // And the rest of the rung's ids are still required.
    expect(draftIncomplete({ ...base, level: "account", accountId: "" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "conversation", conversationId: "" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "global", recipe: " " })).toBe(true);
  });

  it("round-trips every scope and target field through the editor", () => {
    const options = draftOptions(draftFrom(FULL_ROUTE));
    expect(options).toEqual({
      account_id: "chan-1",
      conversation_id: "C1",
      thread_id: "T1",
      sender_id: "U1",
      kind: null,
      repository: "/work/repo",
      params: ["focus=deps", "depth=3"],
      session_scope: "sender",
      priority: 7,
      reply: false,
      enabled: false,
    });
  });

  it("sends only the ids its rung carries, so a leftover value cannot widen a scope", () => {
    // Opened on a sender route, moved down to Account: the conversation,
    // thread and sender fields still hold their old values, and none of them
    // may be sent.
    const options = draftOptions({ ...draftFrom(FULL_ROUTE), level: "account" });
    expect(options.conversation_id).toBeNull();
    expect(options.thread_id).toBeNull();
    expect(options.sender_id).toBeNull();
    expect(options.account_id).toBe("chan-1");
  });
});

describe("the routes section", () => {
  it("keeps Save disabled until a Thread route names its thread", async () => {
    mockRoutes([FULL_ROUTE]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Add route"));

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "chat" } });
    fireEvent.change(screen.getByLabelText("Applies to"), { target: { value: "thread" } });
    fireEvent.change(screen.getByLabelText("Account"), { target: { value: "chan-1" } });
    fireEvent.change(screen.getByLabelText("Conversation"), { target: { value: "C1" } });

    const save = screen.getByText("Save route").closest("button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Thread"), { target: { value: "T1" } });
    expect(save.disabled).toBe(false);
  });

  it("keeps Save disabled until a Sender route names both its thread and its sender", async () => {
    mockRoutes([FULL_ROUTE]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Add route"));

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "chat" } });
    fireEvent.change(screen.getByLabelText("Applies to"), { target: { value: "sender" } });
    fireEvent.change(screen.getByLabelText("Account"), { target: { value: "chan-1" } });
    fireEvent.change(screen.getByLabelText("Conversation"), { target: { value: "C1" } });
    fireEvent.change(screen.getByLabelText("Sender"), { target: { value: "U1" } });

    const save = screen.getByText("Save route").closest("button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Thread"), { target: { value: "T1" } });
    expect(save.disabled).toBe(false);
  });

  it("carries every field back when an unrelated one is edited", async () => {
    mockRoutes([FULL_ROUTE]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Edit"));

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "triage-2" } });
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_update_route", {
        routeId: "route-1",
        recipe: "triage-2",
        options: {
          account_id: "chan-1",
          conversation_id: "C1",
          thread_id: "T1",
          sender_id: "U1",
          kind: null,
          repository: "/work/repo",
          params: ["focus=deps", "depth=3"],
          session_scope: "sender",
          priority: 7,
          reply: false,
          enabled: false,
        },
      }),
    );
  });

  // Adding an account already creates the first route, in the CLI, and only
  // when there is no route at all. This screen doing it too raced that: both
  // wrote a global-default scope and the loser surfaced "already owns this
  // scope" to an operator who had clicked nothing.
  it("never creates a route on its own, however ready the account looks", async () => {
    mockRoutes([], ["channel-chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    await waitFor(() => expect(invoke).toHaveBeenCalledWith("channels_routes"));
    expect(invoke.mock.calls.some(([command]) => command === "channels_add_route")).toBe(false);
  });

  it("offers the starter route instead, for the operator to ask for", async () => {
    mockRoutes([], ["channel-chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    fireEvent.click(await screen.findByText("Set up a starter task and route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "channel-chat",
        options: {},
      }),
    );
  });

  it("names an existing starter task when setting the first route up in one click", async () => {
    mockRoutes([], ["channel-chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    fireEvent.click(await screen.findByText("Set up a starter task and route"));
    // The global default: no scope flags, so every account reaches it.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "channel-chat",
        options: {},
      }),
    );
    // The task already existed, so nothing wrote a second copy over it.
    expect(invoke.mock.calls.some(([command]) => command === "recipes_save")).toBe(false);
  });

  it("refuses to save a route naming a task that is not there", async () => {
    mockRoutes([FULL_ROUTE], ["chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Edit"));

    // The route points at "triage", which is not saved any more: the editor
    // says so rather than reading as some other task.
    expect(screen.getByText(/No task by this name is saved/)).toBeTruthy();
    const picker = screen.getByLabelText("Task") as HTMLSelectElement;
    expect(picker.value).toBe("triage");
    expect([...picker.options].map((option) => option.value)).toContain("chat");
  });

  it("turns a route off and on and removes it by id", async () => {
    mockRoutes([{ ...FULL_ROUTE, enabled: true }]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    fireEvent.click(await screen.findByText("Disable"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_enable_route", {
        routeId: "route-1",
        enabled: false,
      }),
    );

    mockRoutes([{ ...FULL_ROUTE, enabled: false }]);
    cleanup();
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Enable"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_enable_route", {
        routeId: "route-1",
        enabled: true,
      }),
    );

    fireEvent.click(screen.getByText("Remove"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_remove_route", { routeId: "route-1" }),
    );
  });
});

/**
 * The model a route answers on.
 *
 * The property under test is never "the select changed". A route names a
 * *task*, the model lives in that task's `target:`, and that target is what
 * `daemon::freeze_execution_for` reads when a message arrives. So every
 * assertion here is about the task's target: what the row reads out of it,
 * what gets written back into it, and what happens when the write fails or
 * when the machine can no longer offer what it names.
 *
 * That the runner then resolves the written target is proved on the other
 * side of the wire, in `daemon::channel_route_model_tests`.
 */
describe("the model a route answers on", () => {
  const ROUTE_ON = (recipe: string): ChannelRoute => ({
    ...FULL_ROUTE,
    route_id: `route-${recipe}`,
    target: { ...FULL_ROUTE.target, recipe },
  });

  it("shows the model the route's task actually names", async () => {
    mockRoutes(
      [ROUTE_ON("channel-chat")],
      [discovered("channel-chat", { managed_model: "Qwen2.5-7B" })],
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    // Not "unknown", not the first available model: the one the file holds.
    expect(picker.value).toBe("managed:Qwen2.5-7B");
    expect(picker.selectedOptions[0].textContent).toBe("Qwen2.5-7B");
  });

  it("groups every genuinely selectable model by its backend", async () => {
    mockRoutes(
      [ROUTE_ON("channel-chat")],
      [discovered("channel-chat", { managed_model: "Qwen2.5-7B" })],
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    expect([...picker.querySelectorAll("optgroup")].map((group) => group.label)).toEqual([
      "Local",
      "Ollama",
      "OpenRouter",
    ]);
    expect([...picker.options].map((option) => option.value)).toEqual([
      "managed:Qwen2.5-7B",
      "ollama:qwen3:8b",
      "provider:openrouter/anthropic/claude-sonnet-4",
    ]);
  });

  it("offers an installed model that is not the one currently running", async () => {
    // The chat inventory offers only the active local model, and only while
    // llama-server is ready. A recipe target is resolved later, by a runner
    // that starts the managed runtime itself — so an installed, idle model is
    // a legal choice, and hiding it would hide models the operator has.
    mockRoutes(
      [ROUTE_ON("channel-chat")],
      [discovered("channel-chat", { ollama: "qwen3:8b" })],
      {
        installed: [
          INSTALLED_MODEL,
          { ...INSTALLED_MODEL, id: "Llama-3.1-8B", name: "Llama-3.1-8B" },
          { ...INSTALLED_MODEL, id: "bge-m3", name: "bge-m3", kind: "embedding" },
          { ...INSTALLED_MODEL, id: "not-here", name: "not-here", installed: false },
        ],
      },
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    const local = picker.querySelector('optgroup[label="Local"]') as HTMLElement;
    expect([...local.querySelectorAll("option")].map((option) => option.textContent)).toEqual([
      "Qwen2.5-7B",
      "Llama-3.1-8B",
    ]);
  });

  it("writes the picked model into the task's target and leaves the route alone", async () => {
    mockRoutes(
      [ROUTE_ON("channel-chat")],
      [discovered("channel-chat", { managed_model: "Qwen2.5-7B" })],
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    fireEvent.change(picker, { target: { value: "ollama:qwen3:8b" } });

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("recipes_set_target", {
        name: "channel-chat",
        // Exactly one field, as `RecipeTarget::validate`'s XOR requires.
        target: { ollama: "qwen3:8b" },
      }),
    );
    // The model is not a property of the route, so nothing about the route
    // was rewritten to record it.
    expect(invoke.mock.calls.some(([command]) => command === "channels_update_route")).toBe(false);
    expect(invoke.mock.calls.some(([command]) => command === "recipes_save")).toBe(false);
  });

  it("sends the right target for each of the three backends", async () => {
    for (const [value, target] of [
      ["managed:Qwen2.5-7B", { managed_model: "Qwen2.5-7B" }],
      ["ollama:qwen3:8b", { ollama: "qwen3:8b" }],
      [
        "provider:openrouter/anthropic/claude-sonnet-4",
        { provider: "openrouter", model: "anthropic/claude-sonnet-4" },
      ],
    ] as const) {
      mockRoutes([ROUTE_ON("channel-chat")], [discovered("channel-chat", { local_url: "http://x" })]);
      render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
      fireEvent.change(await modelPicker(), { target: { value } });
      await waitFor(() =>
        expect(invoke).toHaveBeenCalledWith("recipes_set_target", {
          name: "channel-chat",
          target,
        }),
      );
      cleanup();
      invoke.mockReset();
    }
  });

  it("shows the written model, then shows it again after a reload", async () => {
    // A stand-in for the recipe file: `recipes_set_target` writes it and
    // `recipes_list` reads it back, so the control can only move by way of the
    // backend. Nothing here lets the UI show a model it merely asked for.
    let onDisk: RecipeTarget = { managed_model: "Qwen2.5-7B" };
    const backingFile = (command: string, args: unknown) => {
      if (command === "recipes_set_target") {
        onDisk = (args as { target: RecipeTarget }).target;
        return Promise.resolve(discovered("channel-chat", onDisk).recipe);
      }
      if (command === "recipes_list") return Promise.resolve([discovered("channel-chat", onDisk)]);
      return undefined;
    };

    mockRoutes([ROUTE_ON("channel-chat")], [], {}, backingFile);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    const picker = await modelPicker();
    expect(picker.value).toBe("managed:Qwen2.5-7B");

    fireEvent.change(picker, { target: { value: "ollama:qwen3:8b" } });
    // The row moved because the re-listed task says so, not because the select
    // was clicked.
    await waitFor(() =>
      expect((screen.getByLabelText("Model") as HTMLSelectElement).value).toBe("ollama:qwen3:8b"),
    );
    expect(onDisk).toEqual({ ollama: "qwen3:8b" });

    // Closed and reopened: a fresh mount, reading the same file.
    cleanup();
    mockRoutes([ROUTE_ON("channel-chat")], [], {}, backingFile);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    expect((await modelPicker()).value).toBe("ollama:qwen3:8b");
  });

  it("keeps showing a saved model this machine cannot currently offer", async () => {
    mockRoutes(
      [ROUTE_ON("channel-chat")],
      [discovered("channel-chat", { ollama: "deepseek-r1:70b" })],
      // The daemon is up but that tag is gone.
      { ollamaModels: [OLLAMA_MODEL] },
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    // Showing "unknown", or snapping to the first available model, would hide
    // the one thing this row exists to report.
    expect(picker.value).toBe("ollama:deepseek-r1:70b");
    expect(picker.selectedOptions[0].textContent).toContain("Ollama · deepseek-r1:70b");
    expect(picker.selectedOptions[0].textContent).toContain("not available here");
    expect(screen.getByText(/Not available on this machine right now/)).toBeTruthy();
    // And merely opening settings must not have rewritten it to something
    // that is available.
    expect(invoke.mock.calls.some(([command]) => command === "recipes_set_target")).toBe(false);
    // The replacement is still one click away.
    expect([...picker.options].map((option) => option.value)).toContain("ollama:qwen3:8b");
  });

  it("says so, rather than guessing, when the route names no saved task", async () => {
    mockRoutes([ROUTE_ON("gone")], [discovered("channel-chat")]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    expect(await screen.findByText(/This task is not saved, so it has no model to show/)).toBeTruthy();
    expect(screen.queryByLabelText("Model")).toBeNull();
  });

  it("leaves the control on the model the runner will really use when the write fails", async () => {
    mockRoutes(
      [ROUTE_ON("channel-chat")],
      [discovered("channel-chat", { managed_model: "Qwen2.5-7B" })],
      {},
      (command) =>
        command === "recipes_set_target"
          ? Promise.reject(new Error("Failed to write recipe: disk is read-only"))
          : undefined,
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    fireEvent.change(picker, { target: { value: "ollama:qwen3:8b" } });

    // The failure is shown, and the control still names what is on disk — a
    // picker that moved here would claim a model the runner is not going to
    // use.
    expect(await screen.findByText(/disk is read-only/)).toBeTruthy();
    expect(picker.value).toBe("managed:Qwen2.5-7B");
  });

  it("says out loud when other routes answer on the same task", async () => {
    // The model lives in the task, so this is not a bug to hide — it is the
    // consequence of two routes naming one task, and the operator is told
    // before they change it rather than after.
    mockRoutes(
      [ROUTE_ON("channel-chat"), { ...ROUTE_ON("channel-chat"), route_id: "route-2" }],
      [discovered("channel-chat", { ollama: "qwen3:8b" })],
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    await waitFor(() => expect(screen.getAllByLabelText("Model")).toHaveLength(2));
    expect(screen.getAllByText("2 routes use this task, so they all answer on this model.")).toHaveLength(2);
  });

  it("does not claim sharing when each route has its own task", async () => {
    mockRoutes(
      [ROUTE_ON("channel-chat"), ROUTE_ON("channel-triage")],
      [
        discovered("channel-chat", { ollama: "qwen3:8b" }),
        discovered("channel-triage", { managed_model: "Qwen2.5-7B" }),
      ],
    );
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    await waitFor(() => expect(screen.getAllByLabelText("Model")).toHaveLength(2));
    expect(screen.queryByText(/routes use this task/)).toBeNull();
    expect(
      (screen.getAllByLabelText("Model") as HTMLSelectElement[]).map((picker) => picker.value),
    ).toEqual(["ollama:qwen3:8b", "managed:Qwen2.5-7B"]);
  });

  it("still renders the route list when this machine has no models at all", async () => {
    mockRoutes([ROUTE_ON("channel-chat")], [discovered("channel-chat", { ollama: "qwen3:8b" })], {
      installed: [],
      ollamaReachable: false,
      ollamaModels: [],
      providers: [],
      providerModels: {},
    });
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = (await screen.findByLabelText("Model")) as HTMLSelectElement;
    expect(picker.value).toBe("ollama:qwen3:8b");
    expect(await screen.findByText(/No models are available yet/)).toBeTruthy();
  });

  it("omits a provider with no stored key, and Ollama when its daemon is down", async () => {
    mockRoutes([ROUTE_ON("channel-chat")], [discovered("channel-chat", { managed_model: "Qwen2.5-7B" })], {
      ollamaReachable: false,
      providers: [{ ...OPENROUTER, has_key: false }],
    });
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    const picker = await modelPicker();
    expect([...picker.options].map((option) => option.value)).toEqual(["managed:Qwen2.5-7B"]);
  });
});
