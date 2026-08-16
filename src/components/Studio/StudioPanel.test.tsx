// @vitest-environment jsdom
/**
 * Undo and redo for "Extend the picture".
 *
 * An extension moves three things at once — the source image, the mask and the
 * requested size — so stepping back is only correct if the size comes back with
 * the picture. That pairing is what this drives, through the panel rather than
 * around it, because the stacks live in the panel's own state.
 *
 * Everything below the buttons is stubbed: the engine (`studioClient`), the
 * outpaint call, the file picker, and the canvas/image APIs jsdom does not
 * implement.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import type { GenerationModel } from "../../lib/studioClient";

const runOutpaint = vi.fn();

vi.mock("../../lib/imageAttachment", () => ({
  pickImageBase64: vi.fn(async () => "SOURCE"),
}));

vi.mock("../../lib/outpaint", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/outpaint")>()),
  runOutpaint,
}));

vi.mock("../../lib/studioClient", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/studioClient")>();
  return {
    ...actual,
    studioClient: {
      engineStatus: async () => ({
        supported: true,
        engineInstalled: true,
        loadedModelId: null,
        totalRamBytes: 32_000_000_000,
      }),
      models: async () => [MODEL],
      gallery: async () => [],
      loras: async () => [],
      parts: async () => [],
      backends: async () => [],
      capabilities: async () => null,
      onProgress: async () => () => {},
    },
  };
});

const MODEL: GenerationModel = {
  id: "test-model",
  name: "Test model",
  family: "sd1",
  // Only the one task, so the panel opens on `image_to_image` — which is what
  // puts the init image, the mask and the extension controls on screen.
  tasks: ["image_to_image"],
  components: [],
  defaults: {
    width: 512,
    height: 512,
    steps: 20,
    cfgScale: 7,
    sampleMethod: "euler_a",
    flowShift: null,
    fps: 8,
    videoFrames: 16,
    frameGrid: "down_to4n_plus1",
  },
  minRamBytes: 0,
  license: {
    id: "test",
    name: "Test",
    url: "",
    excludedTerritories: [],
    acceptanceRequired: false,
  },
  extraLaunchArgs: [],
  installed: true,
  totalBytes: 2_000_000_000,
  missingBytes: 0,
  licenseAccepted: true,
  fitsInMemory: true,
};

/** jsdom never fires `load` for a `data:` URL. */
class StubImage {
  onload: (() => void) | null = null;
  naturalWidth = 512;
  naturalHeight = 512;
  set src(_value: string) {
    queueMicrotask(() => this.onload?.());
  }
}

// The module reads this once, at import time, to decide whether it is running
// inside the desktop window — so it has to be set before the import below.
(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
const { StudioPanel } = await import("./StudioPanel");

/** The mask preview renders the current source image, which is the only place
 *  the panel shows which picture it is holding. */
const shownImage = () =>
  document.querySelector<HTMLImageElement>('img[src^="data:image/png;base64,"]')?.src;

const widthValue = () => (screen.getByLabelText("Width") as HTMLInputElement).value;

describe("StudioPanel extension history", () => {
  let rail: HTMLElement;

  beforeEach(() => {
    runOutpaint.mockReset();
    HTMLCanvasElement.prototype.getContext = vi.fn(() => ({
      fillStyle: "",
      strokeStyle: "",
      lineCap: "",
      lineJoin: "",
      lineWidth: 0,
      fillRect: vi.fn(),
      beginPath: vi.fn(),
      moveTo: vi.fn(),
      lineTo: vi.fn(),
      stroke: vi.fn(),
      arc: vi.fn(),
      fill: vi.fn(),
      drawImage: vi.fn(),
    })) as never;
    HTMLCanvasElement.prototype.toDataURL = vi.fn(() => "data:image/png;base64,MASK") as never;
    vi.stubGlobal("Image", StubImage);
    // jsdom ships `<dialog>` without its methods; the lightbox effect calls
    // them on every render.
    HTMLDialogElement.prototype.showModal = vi.fn();
    HTMLDialogElement.prototype.close = vi.fn();
    // The settings rail is portalled into a node the app owns; the test plays
    // that part, and it has to be in the document for the controls to be found.
    rail = document.createElement("div");
    document.body.append(rail);
  });

  afterEach(() => {
    cleanup();
    rail.remove();
    vi.unstubAllGlobals();
  });

  /** Renders the panel, waits for the library to arrive, and picks a source
   *  image — the state every extension starts from. */
  async function setup() {
    render(<StudioPanel mode="image" railSlot={rail} />);
    await waitFor(() => expect(screen.getByText("Choose image")).toBeTruthy());
    fireEvent.click(screen.getByText("Choose image"));
    await waitFor(() => expect(shownImage()).toContain("SOURCE"));
  }

  const undo = () => screen.getByLabelText("Undo the last extension");
  const redo = () => screen.getByLabelText("Redo the undone extension");

  it("has nothing to step through until something is extended", async () => {
    await setup();
    expect(undo().hasAttribute("disabled")).toBe(true);
    expect(redo().hasAttribute("disabled")).toBe(true);
  });

  it("puts the picture and the size back, then forward again", async () => {
    await setup();
    runOutpaint.mockResolvedValue({
      initImageBase64: "EXTENDED",
      maskImageBase64: "MASK",
      width: 640,
      height: 512,
    });

    fireEvent.click(screen.getByLabelText("Extend to the right"));
    await waitFor(() => expect(shownImage()).toContain("EXTENDED"));
    expect(widthValue()).toBe("640");
    expect(redo().hasAttribute("disabled")).toBe(true);

    fireEvent.click(undo());
    await waitFor(() => expect(shownImage()).toContain("SOURCE"));
    // The size follows the picture: an undone extension that left the form at
    // 640 would hand the engine a canvas the image does not fill.
    expect(widthValue()).toBe("512");
    expect(undo().hasAttribute("disabled")).toBe(true);

    fireEvent.click(redo());
    await waitFor(() => expect(shownImage()).toContain("EXTENDED"));
    expect(widthValue()).toBe("640");
    expect(redo().hasAttribute("disabled")).toBe(true);
  });

  it("drops the redo stack once a fresh extension is made", async () => {
    await setup();
    runOutpaint.mockResolvedValue({
      initImageBase64: "FIRST",
      maskImageBase64: "MASK",
      width: 640,
      height: 512,
    });

    fireEvent.click(screen.getByLabelText("Extend to the right"));
    await waitFor(() => expect(shownImage()).toContain("FIRST"));
    fireEvent.click(undo());
    await waitFor(() => expect(redo().hasAttribute("disabled")).toBe(false));

    runOutpaint.mockResolvedValue({
      initImageBase64: "SECOND",
      maskImageBase64: "MASK",
      width: 512,
      height: 640,
    });
    fireEvent.click(screen.getByLabelText("Extend downwards"));
    await waitFor(() => expect(shownImage()).toContain("SECOND"));
    expect(redo().hasAttribute("disabled")).toBe(true);
  });

  it("forgets both stacks when the source image is replaced", async () => {
    await setup();
    runOutpaint.mockResolvedValue({
      initImageBase64: "EXTENDED",
      maskImageBase64: "MASK",
      width: 640,
      height: 512,
    });

    fireEvent.click(screen.getByLabelText("Extend to the right"));
    await waitFor(() => expect(undo().hasAttribute("disabled")).toBe(false));

    fireEvent.click(screen.getByText("Choose image"));
    await waitFor(() => expect(shownImage()).toContain("SOURCE"));
    expect(undo().hasAttribute("disabled")).toBe(true);
    expect(redo().hasAttribute("disabled")).toBe(true);
  });

  /** The engine renders on a 32-pixel grid. A field left holding a number off
   *  that grid reports a canvas the run never had. */
  it("holds the size the engine will really render", async () => {
    await setup();
    // The label holds both controls; typing happens in the number one.
    const typed = screen
      .getByLabelText("Width")
      .parentElement!.querySelector<HTMLInputElement>('input[type="number"]')!;
    fireEvent.change(typed, { target: { value: "645" } });
    fireEvent.blur(typed);
    // Nearest, not up: 645 is answered with 640 rather than the 672 the
    // backend would have rounded it to.
    await waitFor(() => expect(widthValue()).toBe("640"));
  });

  it("confirms a typed size on Enter, without leaving the field", async () => {
    await setup();
    const typed = screen
      .getByLabelText("Width")
      .parentElement!.querySelector<HTMLInputElement>('input[type="number"]')!;
    fireEvent.change(typed, { target: { value: "700" } });
    fireEvent.keyDown(typed, { key: "Enter" });
    await waitFor(() => expect(widthValue()).toBe("704"));
  });

  it("takes the canvas from the source image's own size", async () => {
    vi.stubGlobal(
      "Image",
      class extends StubImage {
        naturalWidth = 645;
        naturalHeight = 890;
      },
    );
    await setup();
    fireEvent.click(screen.getByLabelText("Original size of the source image"));
    await waitFor(() => expect(widthValue()).toBe("640"));
  });
});
