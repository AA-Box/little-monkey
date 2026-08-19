// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const client = vi.hoisted(() => ({
  addModel: vi.fn(),
  setHuggingFaceToken: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("../../lib/i18n", () => ({
  useT: () => ({ t: (key: string) => key, locale: "en-US" }),
}));
vi.mock("../../lib/studioClient", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/studioClient")>();
  return {
    ...actual,
    studioClient: {
      ...actual.studioClient,
      addModel: client.addModel,
      setHuggingFaceToken: client.setHuggingFaceToken,
    },
  };
});
vi.mock("./ModelFiles", () => ({
  ModelFiles: () => <div data-testid="model-files" />,
}));

import { AddModelForm } from "./AddModelForm";

describe("AddModelForm MFLUX mode", () => {
  it("shows the repository and quantization controls and hides component files", () => {
    render(<AddModelForm onSaved={vi.fn()} />);

    fireEvent.change(screen.getAllByRole("combobox")[1], {
      target: { value: "mflux_image" },
    });

    expect(screen.getByDisplayValue("black-forest-labs/FLUX.1-dev")).toBeTruthy();
    expect(screen.getByRole("option", { name: "8-bit" })).toHaveProperty("selected", true);
    expect(screen.queryByTestId("model-files")).toBeNull();
    expect(screen.getByText("Studio.add.mfluxToken")).toBeTruthy();
  });
});
