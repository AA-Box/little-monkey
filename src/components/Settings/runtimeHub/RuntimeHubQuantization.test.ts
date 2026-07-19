import { describe, expect, it } from "vitest";

import type { QuantTypeDescriptor } from "../../../lib/runtimeHubClient";
import { licenseRiskTone, pickDefaultQuantType } from "./RuntimeHubQuantization";

describe("Runtime Hub quantization workbench helpers", () => {
  it("maps license risk to a pill tone matching its severity", () => {
    expect(licenseRiskTone("permissive")).toBe("success");
    expect(licenseRiskTone("copyleft")).toBe("warning");
    expect(licenseRiskTone("restricted")).toBe("danger");
    expect(licenseRiskTone("unknown")).toBe("neutral");
  });

  it("prefers Q4_K_M as the default quantization choice when offered", () => {
    const quantTypes: QuantTypeDescriptor[] = [
      { id: "COPY", cliName: "COPY", note: "No quantization." },
      { id: "Q8_0", cliName: "Q8_0", note: "Near-lossless." },
      { id: "Q4_K_M", cliName: "Q4_K_M", note: "Balanced default." },
    ];
    expect(pickDefaultQuantType(quantTypes)).toBe("Q4_K_M");
  });

  it("falls back to the first entry when Q4_K_M isn't offered, and to empty when the list is empty", () => {
    const quantTypes: QuantTypeDescriptor[] = [
      { id: "COPY", cliName: "COPY", note: "No quantization." },
      { id: "Q8_0", cliName: "Q8_0", note: "Near-lossless." },
    ];
    expect(pickDefaultQuantType(quantTypes)).toBe("COPY");
    expect(pickDefaultQuantType([])).toBe("");
  });
});
