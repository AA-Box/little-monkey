// @vitest-environment jsdom
/**
 * The remote-handoff tab, driven the way an operator drives it.
 *
 * Two claims are load-bearing here, and neither is about layout.
 *
 * A pairing invitation is **one-time**: the code an operator scans and the file
 * they transfer carry the same token, so the code has to be shown at the moment
 * the invitation is created. Asking for it afterwards would mean creating a
 * second invitation and stranding the first, which is why the panel renders it
 * immediately and why this test pins that it does.
 *
 * Push is **the operator's own configuration** — Little Monkey ships no push
 * project, no key and no relay. What matters is that the panel reaches the
 * typed bridge with exactly what was chosen, that nothing widens it, and that
 * "specifics on a lock screen" is a decision somebody makes out loud rather
 * than a default.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: () => Promise.resolve("/keys/service-account.json"),
  save: () => Promise.resolve("/invites/little-monkey-pairing.json"),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: () => Promise.resolve() }));
vi.mock("../../store/recipeStore", () => ({
  useRecipeStore: (selector: (state: unknown) => unknown) =>
    selector({ recipes: [], refresh: () => Promise.resolve() }),
}));
vi.mock("../../store/runStore", () => ({
  useRunStore: (selector: (state: unknown) => unknown) =>
    selector({
      runs: [],
      eventsByRun: {},
      selectedRunId: null,
      refresh: () => Promise.resolve(),
      selectRun: () => undefined,
    }),
}));

import { BackgroundAgentsPanel } from "./BackgroundAgentsPanel";

const SVG = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 85 85"><rect/></svg>';

const PUSH = {
  configured: true,
  enabled: true,
  backend: "web_push",
  project_id: null,
  application_server_key: "BEl62iUYgUivxIkv69yViEuiBIa40HcCWLEHpMbZ4Cr8".repeat(1),
  include_detail: false,
  registered_devices: [{ device_id: "device-phone", backend: "web_push" }],
};

function mock(overrides: Record<string, unknown> = {}) {
  invoke.mockImplementation((command: string, args?: Record<string, unknown>) => {
    if (command === "daemon_desktop_status") {
      return Promise.resolve({ installed: true, serviceRunning: true, authority: "daemon" });
    }
    if (command === "remote_pair_create") {
      return Promise.resolve({
        invitation_path: "/invites/little-monkey-pairing.json",
        controller_url: "https://runner.example.net/remote",
        expires_at_ms: 4_000_000_000_000,
        bootstrap_uri: "littlemonkey://pair/QUJD",
        bootstrap_bytes: 331,
        qr_svg: SVG,
        qr_modules: 77,
        ...(overrides.pair as object | undefined),
      });
    }
    if (command === "remote_push_status") return Promise.resolve({ ...PUSH, ...(overrides.push as object | undefined) });
    if (command === "remote_push_configure") return Promise.resolve("Web Push is on.");
    if (command === "remote_push_test") return Promise.resolve("Sent to device-phone.");
    if (command === "remote_push_disable") return Promise.resolve("Push disabled.");
    if (command === "remote_device_list") return Promise.resolve({ devices: [] });
    void args;
    return Promise.resolve({});
  });
}

async function openRemoteTab() {
  render(<BackgroundAgentsPanel />);
  fireEvent.click(await screen.findByRole("tab", { name: /remote handoff/i }));
}

/** The push card, found by its heading rather than by position: several panels
 * on this tab have a Refresh button, and an index would silently start testing
 * a different one the next time a card is added. */
function pushCard() {
  const heading = screen.getByRole("heading", { name: /notifications to paired devices/i });
  return heading.closest("div.rounded-lg") as HTMLElement;
}

beforeEach(() => {
  invoke.mockReset();
  mock();
  vi.spyOn(window, "confirm").mockReturnValue(true);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("pairing with a compact code", () => {
  it("shows the scannable code and its paste form as soon as the invitation exists", async () => {
    await openRemoteTab();
    // The invitation needs a declared scope before it can be created at all.
    fireEvent.change(screen.getByLabelText(/allowed run ids/i), { target: { value: "run-one" } });
    fireEvent.click(screen.getByRole("button", { name: /create invitation/i }));

    const image = await screen.findByRole("img", { name: /pairing code/i });
    // Rendered as a data URI rather than injected markup: the node produced
    // this SVG, and an <img> cannot execute anything inside it.
    expect(image.getAttribute("src")).toBe(`data:image/svg+xml;base64,${btoa(SVG)}`);
    // The same one-time token, in the form somebody without a camera can use.
    expect(screen.getByText("littlemonkey://pair/QUJD")).toBeTruthy();
    expect(screen.getByText(/331 bytes/)).toBeTruthy();

    const [command, payload] = invoke.mock.calls.find(([name]) => name === "remote_pair_create")!;
    expect(command).toBe("remote_pair_create");
    expect((payload as { request: { output: string } }).request.output).toBe(
      "/invites/little-monkey-pairing.json",
    );
  });

  it("hides the code on request, because it is a live pairing secret", async () => {
    await openRemoteTab();
    fireEvent.change(screen.getByLabelText(/allowed run ids/i), { target: { value: "run-one" } });
    fireEvent.click(screen.getByRole("button", { name: /create invitation/i }));
    await screen.findByRole("img", { name: /pairing code/i });
    fireEvent.click(screen.getByRole("button", { name: /hide code/i }));
    await waitFor(() => expect(screen.queryByRole("img", { name: /pairing code/i })).toBeNull());
  });
});

describe("push settings", () => {
  it("separates configured, enabled and what a notification would say", async () => {
    await openRemoteTab();
    fireEvent.click(within(pushCard()).getByRole("button", { name: /^refresh$/i }));
    await screen.findByText(/enabled · web_push/);
    expect(screen.getByText(/withheld — kind and id only/)).toBeTruthy();
    // Never the device's token, which is an address rather than a diagnostic.
    expect(screen.queryByText(/BEl62iUYgUivxIkv69yViEuiBIa40HcCWLEHpMbZ4Cr8$/)).toBeNull();
  });

  it("sends exactly the backend the operator chose, and no Firebase fields with Web Push", async () => {
    await openRemoteTab();
    fireEvent.click(screen.getByRole("button", { name: /save push settings/i }));
    await waitFor(() =>
      expect(invoke.mock.calls.some(([name]) => name === "remote_push_configure")).toBe(true),
    );
    const [, payload] = invoke.mock.calls.find(([name]) => name === "remote_push_configure")!;
    expect(payload).toEqual({
      webPush: true,
      vapidSubject: null,
      projectId: null,
      serviceAccount: null,
      includeDetail: false,
    });
  });

  it("carries the operator's own Firebase project through unchanged", async () => {
    await openRemoteTab();
    fireEvent.click(screen.getByRole("radio", { name: /firebase project/i }));
    fireEvent.change(screen.getByLabelText(/firebase project id/i), {
      target: { value: "my-own-project" },
    });
    fireEvent.click(screen.getByRole("button", { name: /service account key/i }));
    await screen.findByText("/keys/service-account.json");
    fireEvent.click(screen.getByLabelText(/lock screen/i));
    fireEvent.click(screen.getByRole("button", { name: /save push settings/i }));
    await waitFor(() =>
      expect(invoke.mock.calls.some(([name]) => name === "remote_push_configure")).toBe(true),
    );
    const [, payload] = invoke.mock.calls.find(([name]) => name === "remote_push_configure")!;
    expect(payload).toEqual({
      webPush: false,
      vapidSubject: null,
      projectId: "my-own-project",
      serviceAccount: "/keys/service-account.json",
      includeDetail: true,
    });
  });

  it("offers a test push per registered device and names which one", async () => {
    await openRemoteTab();
    fireEvent.click(within(pushCard()).getByRole("button", { name: /^refresh$/i }));
    const test = await screen.findByRole("button", { name: /test push to device-phone/i });
    fireEvent.click(test);
    await waitFor(() =>
      expect(invoke.mock.calls.some(([name]) => name === "remote_push_test")).toBe(true),
    );
    const [, payload] = invoke.mock.calls.find(([name]) => name === "remote_push_test")!;
    expect(payload).toEqual({ deviceId: "device-phone" });
  });
});
