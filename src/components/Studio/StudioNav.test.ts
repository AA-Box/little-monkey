/**
 * The nav renders `t(labelKey)` with a variable, so the i18n key-lint — which
 * only scans call sites whose key is written out as a string literal — cannot
 * see these four keys. A typo would ship as a raw key string in the sidebar with
 * nothing failing, so check them here instead.
 *
 * Coverage of the `StudioMode` union needs no test: the union is derived from
 * `STUDIO_MODES`, so an unreachable mode cannot be expressed.
 */
import { describe, expect, it } from "vitest";

import { STUDIO_MODES } from "./StudioNav";
import { en } from "../../lib/i18n/locales/en";

describe("STUDIO_MODES", () => {
  it("names a label key that English actually defines", () => {
    for (const { id, labelKey } of STUDIO_MODES) {
      expect(en[labelKey as keyof typeof en], `${id} → ${labelKey}`).toBeTruthy();
    }
  });

  it("lists every section once", () => {
    const ids = STUDIO_MODES.map((mode) => mode.id);
    expect(ids).toEqual(["image", "video", "audio", "models"]);
    expect(new Set(ids).size).toBe(ids.length);
  });
});
