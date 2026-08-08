/**
 * The parts of the Studio client that decide what the generation form offers.
 *
 * `availableConditioning` is what stands between a user and a three-minute
 * render that quietly ignored the image they gave it: `sd-server` accepts
 * `control_image` whether or not `--control-net` was loaded and simply drops it
 * when it was not. The backend refuses that combination too, so this is the
 * *offer* side of the same rule — and the two agreeing is the point.
 */
import { describe, expect, it } from "vitest";

import {
  availableConditioning,
  CONDITIONING_SLOTS,
  COMPONENT_SLOTS,
  type ComponentSlot,
} from "./studioClient";

describe("availableConditioning", () => {
  it("unlocks an image only when the weights that read it are loaded", () => {
    expect(availableConditioning(["checkpoint", "vae"])).toEqual(new Set());
    expect(availableConditioning(["checkpoint", "control_net"])).toEqual(new Set(["control"]));
    expect(availableConditioning(["ip_adapter", "clip_vision"])).toEqual(new Set(["ip_adapter"]));
  });

  it("treats PhotoMaker and PuLID as two routes to the same reference input", () => {
    expect(availableConditioning(["photo_maker"])).toEqual(new Set(["reference"]));
    expect(availableConditioning(["pulid_weights"])).toEqual(new Set(["reference"]));
    // Both loaded is still one input, not two.
    expect(availableConditioning(["photo_maker", "pulid_weights"])).toEqual(
      new Set(["reference"]),
    );
  });

  it("keeps the three kinds distinct", () => {
    // A ControlNet does not stand in for an IP-Adapter: the engine reads the
    // two from different request fields with different weights.
    expect(availableConditioning(["control_net"]).has("ip_adapter")).toBe(false);
    expect(availableConditioning(["control_net", "ip_adapter", "photo_maker"])).toEqual(
      new Set(["control", "ip_adapter", "reference"]),
    );
  });
});

describe("COMPONENT_SLOTS", () => {
  it("gives every slot exactly one engine flag", () => {
    const slots = COMPONENT_SLOTS.map((entry) => entry.slot);
    expect(new Set(slots).size).toBe(slots.length);
    const flags = COMPONENT_SLOTS.map((entry) => entry.flag);
    expect(new Set(flags).size).toBe(flags.length);
    for (const flag of flags) expect(flag.startsWith("--")).toBe(true);
  });

  it("can offer every conditioning slot in the Models tab", () => {
    // A conditioning slot missing from this table is a weight file the user has
    // no way to attach, which makes the matching input unreachable rather than
    // merely undocumented.
    const listed = new Set<ComponentSlot>(COMPONENT_SLOTS.map((entry) => entry.slot));
    for (const slot of Object.keys(CONDITIONING_SLOTS) as ComponentSlot[]) {
      expect(listed.has(slot)).toBe(true);
    }
  });
});
