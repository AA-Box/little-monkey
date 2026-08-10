// @vitest-environment jsdom
/**
 * The profile switcher's one dangerous affordance: switching restarts the app,
 * and deleting removes a whole data root. Both are gated on a confirm, and the
 * gate is what this drives — a switch that fires on the first click would end
 * a running chat without asking.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

import { ProfilesPanel, type ProfileSummary } from "./ProfilesPanel";

const UNBOUNDED = { maxConcurrentRuns: null, maxMemoryBytes: null, maxRuntimeMs: null };

const PROFILES: ProfileSummary[] = [
  {
    id: "default",
    name: "Default",
    createdAtMs: 0,
    fairShareWeight: 1,
    quota: UNBOUNDED,
    active: true,
    root: "/data/com.littlemonkey.app",
    share: 0.5,
  },
  {
    id: "work",
    name: "Work",
    createdAtMs: 1,
    fairShareWeight: 1,
    quota: UNBOUNDED,
    active: false,
    root: "/data/com.littlemonkey.app/profiles/work",
    share: 0.5,
  },
];

beforeEach(() => {
  invoke.mockReset();
  invoke.mockImplementation((command: string) =>
    command === "profiles_list" ? Promise.resolve(PROFILES) : Promise.resolve(PROFILES),
  );
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ProfilesPanel", () => {
  it("lists every profile with its own data root and share", async () => {
    render(<ProfilesPanel />);

    await waitFor(() => expect(screen.getByText("Work")).toBeTruthy());
    expect(screen.getByText(/profiles\/work/)).toBeTruthy();
    expect(screen.getAllByText("50% share").length).toBe(2);
    // The active profile offers no Switch button — there is nothing to switch to.
    expect(screen.getAllByText("Switch").length).toBe(1);
  });

  it("does not switch — and therefore does not restart — until the confirm is accepted", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<ProfilesPanel />);
    await waitFor(() => expect(screen.getByText("Work")).toBeTruthy());

    fireEvent.click(screen.getByText("Switch"));
    expect(confirm).toHaveBeenCalled();
    expect(invoke.mock.calls.some(([command]) => command === "profiles_switch")).toBe(false);

    confirm.mockReturnValue(true);
    fireEvent.click(screen.getByText("Switch"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("profiles_switch", { id: "work" }),
    );
  });

  it("sends a quota with the unit conversions the backend expects", async () => {
    render(<ProfilesPanel />);
    await waitFor(() => expect(screen.getByText("Work")).toBeTruthy());

    const memory = screen.getAllByLabelText("Max memory (MB)")[1] as HTMLInputElement;
    const runtime = screen.getAllByLabelText("Max run time (s)")[1] as HTMLInputElement;
    fireEvent.change(memory, { target: { value: "512" } });
    fireEvent.change(runtime, { target: { value: "30" } });
    fireEvent.click(screen.getAllByText("Apply limits")[1]);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("profiles_set_limits", {
        id: "work",
        quota: {
          maxConcurrentRuns: null,
          maxMemoryBytes: 512 * 1024 * 1024,
          maxRuntimeMs: 30_000,
        },
        fairShareWeight: 1,
      }),
    );
  });
});
