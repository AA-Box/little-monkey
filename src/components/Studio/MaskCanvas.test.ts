/**
 * The zoom stepping, which is the one part of the mask painter that is pure.
 *
 * Clamping is what these are for: the buttons are disabled at each end, but a
 * helper that walked off the list would hand the container a zoom with no stop
 * behind it and strand the user at a magnification they cannot leave.
 */
import { describe, expect, it } from "vitest";

import { nextStop, previousStop } from "./MaskCanvas";

describe("zoom stepping", () => {
  it("steps up and back down through the stops", () => {
    expect(nextStop(1)).toBe(2);
    expect(nextStop(2)).toBe(3);
    expect(previousStop(3)).toBe(2);
    expect(previousStop(2)).toBe(1);
  });

  it("clamps at both ends rather than walking off the list", () => {
    expect(previousStop(1)).toBe(1);
    expect(nextStop(4)).toBe(4);
  });

  it("lands on a real stop from a value between two", () => {
    // Nothing sets a fractional zoom today, but the helpers decide where a
    // value goes rather than assuming it is already a stop.
    expect(nextStop(2.5)).toBe(3);
    expect(previousStop(2.5)).toBe(2);
  });

  it("recovers from a value outside the list entirely", () => {
    expect(nextStop(99)).toBe(4);
    expect(previousStop(-5)).toBe(1);
  });
});
