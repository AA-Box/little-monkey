import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import { synchronizeRecipeSchedules } from "./recipeScheduleClient";

beforeEach(() => invokeMock.mockReset());

describe("recipe schedule client", () => {
  it("sends the complete typed snapshot through the fixed daemon bridge", async () => {
    invokeMock.mockResolvedValue({ authority: "daemon" });
    const schedules = [{
      entryId: "entry-one",
      recipeName: "nightly",
      recipePath: "/workspace/.littlemonkey/recipes/nightly.yml",
      cron: "0 3 * * *",
      enabled: true,
      permissionModeOverride: null,
    }];

    await synchronizeRecipeSchedules(schedules);

    expect(invokeMock).toHaveBeenCalledWith("daemon_desktop_sync_recipe_schedules", {
      request: { schedules },
    });
  });
});
