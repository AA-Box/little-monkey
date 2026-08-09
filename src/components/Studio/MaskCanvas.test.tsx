// @vitest-environment jsdom
/**
 * Stroke history for the inpainting mask.
 *
 * Rendered rather than unit-tested around: undo is only correct if the *pre*-
 * stroke mask is what comes back, and that snapshot is taken on `pointerdown`
 * and consumed on `pointerup` — a seam that only exists when the component is
 * actually driven by pointer events.
 *
 * jsdom has no 2D context and does not decode images, so both are stubbed: the
 * canvas hands back a recording context and a fresh `toDataURL` string per
 * stroke, which is enough to tell one committed mask from another.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";

import { MaskCanvas, nextStop, previousStop } from "./MaskCanvas";

/** Incremented per export so each stroke commits a distinguishable mask. */
let exportCount = 0;

function stubCanvas() {
  exportCount = 0;
  const context = {
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
  };
  HTMLCanvasElement.prototype.getContext = vi.fn(() => context) as never;
  HTMLCanvasElement.prototype.toDataURL = vi.fn(
    () => `data:image/png;base64,MASK${++exportCount}`,
  ) as never;
  return context;
}

/** jsdom never fires `load` for a `data:` URL, and the component waits for it
 *  before it will size the canvas or paint a restored mask. */
class StubImage {
  onload: (() => void) | null = null;
  naturalWidth = 64;
  naturalHeight = 48;
  set src(_value: string) {
    queueMicrotask(() => this.onload?.());
  }
}

/** The component is controlled, so the test owns the mask the same way
 *  `StudioPanel` does — undo is only meaningful against a parent that keeps
 *  what was committed. */
function Harness() {
  const [mask, setMask] = useState<string | null>(null);
  return (
    <>
      <MaskCanvas imageBase64="SOURCE" value={mask} onChange={setMask} />
      <output data-testid="mask">{mask ?? "none"}</output>
    </>
  );
}

const maskValue = () => screen.getByTestId("mask").textContent;

function paintStroke(canvas: HTMLCanvasElement) {
  fireEvent.pointerDown(canvas, { clientX: 4, clientY: 4 });
  fireEvent.pointerMove(canvas, { clientX: 8, clientY: 8 });
  fireEvent.pointerUp(canvas);
}

describe("MaskCanvas stroke history", () => {
  beforeEach(() => {
    stubCanvas();
    vi.stubGlobal("Image", StubImage);
    Element.prototype.setPointerCapture = vi.fn();
    Element.prototype.releasePointerCapture = vi.fn();
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  /** Renders and waits for the source image to "load", which is what sizes the
   *  canvas and puts the component in its empty state. */
  async function setup() {
    const view = render(<Harness />);
    const canvas = view.container.querySelector("canvas");
    if (!canvas) throw new Error("no canvas rendered");
    await waitFor(() => expect(screen.getByText(/64.*48/)).toBeTruthy());
    return canvas;
  }

  const undo = () => screen.getByLabelText("Undo the last stroke");
  const redo = () => screen.getByLabelText("Redo the undone stroke");

  it("starts with nothing to undo or redo", async () => {
    await setup();
    expect(undo().hasAttribute("disabled")).toBe(true);
    expect(redo().hasAttribute("disabled")).toBe(true);
  });

  it("takes back one stroke at a time and puts each one back", async () => {
    const canvas = await setup();

    paintStroke(canvas);
    expect(maskValue()).toBe("MASK1");
    paintStroke(canvas);
    expect(maskValue()).toBe("MASK2");
    expect(redo().hasAttribute("disabled")).toBe(true);

    fireEvent.click(undo());
    expect(maskValue()).toBe("MASK1");
    fireEvent.click(undo());
    expect(maskValue()).toBe("none");
    expect(undo().hasAttribute("disabled")).toBe(true);

    fireEvent.click(redo());
    expect(maskValue()).toBe("MASK1");
    fireEvent.click(redo());
    expect(maskValue()).toBe("MASK2");
    expect(redo().hasAttribute("disabled")).toBe(true);
  });

  it("drops the redo stack once a new stroke is painted", async () => {
    const canvas = await setup();

    paintStroke(canvas);
    fireEvent.click(undo());
    expect(redo().hasAttribute("disabled")).toBe(false);

    paintStroke(canvas);
    expect(redo().hasAttribute("disabled")).toBe(true);
    expect(maskValue()).toBe("MASK2");
  });

  it("makes clearing undoable", async () => {
    const canvas = await setup();

    paintStroke(canvas);
    fireEvent.click(screen.getByText("Clear"));
    expect(maskValue()).toBe("none");

    fireEvent.click(undo());
    expect(maskValue()).toBe("MASK1");
  });

  it("repaints the canvas from the restored mask", async () => {
    const context = stubCanvas();
    const canvas = await setup();

    paintStroke(canvas);
    fireEvent.click(undo());
    // Back to the empty mask: cleared to black, with nothing drawn over it.
    expect(context.drawImage).not.toHaveBeenCalled();

    fireEvent.click(redo());
    await waitFor(() => expect(context.drawImage).toHaveBeenCalled());
  });
});

/** The zoom stepping is pure, so it needs no rendering — but the clamping does
 *  need pinning: a helper that walked off the list would hand the container a
 *  magnification with no stop behind it and strand the user there. */
describe("zoom stepping", () => {
  it("steps through the stops and clamps at both ends", () => {
    expect(nextStop(1)).toBe(2);
    expect(previousStop(3)).toBe(2);
    expect(previousStop(1)).toBe(1);
    expect(nextStop(4)).toBe(4);
  });

  it("lands on a real stop from a value between two, or outside them", () => {
    expect(nextStop(2.5)).toBe(3);
    expect(previousStop(2.5)).toBe(2);
    expect(nextStop(99)).toBe(4);
    expect(previousStop(-5)).toBe(1);
  });
});

/**
 * A mask supplied for the image being handed over has to survive.
 *
 * Extending the picture sets a new source *and* the mask marking the new margin
 * in one update. Clearing unconditionally threw that mask away, so the margin
 * reached the engine unmarked and outpainting could not work — the failure was
 * invisible, because the picture really did grow.
 */
describe("MaskCanvas with a mask supplied for the new image", () => {
  beforeEach(() => {
    stubCanvas();
    vi.stubGlobal("Image", StubImage);
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("keeps it, and paints it onto the canvas", async () => {
    const context = stubCanvas();
    vi.stubGlobal("Image", StubImage);
    const onChange = vi.fn();
    render(<MaskCanvas imageBase64="EXTENDED" value="MARGIN_MASK" onChange={onChange} />);

    await waitFor(() => expect(context.drawImage).toHaveBeenCalled());
    // Never cleared: reporting null here is what dropped the margin.
    expect(onChange).not.toHaveBeenCalledWith(null);
  });

  it("still clears when the parent has no mask for the new image", async () => {
    const onChange = vi.fn();
    render(<MaskCanvas imageBase64="FRESH" value={null} onChange={onChange} />);

    await waitFor(() => expect(onChange).toHaveBeenCalledWith(null));
  });
});
