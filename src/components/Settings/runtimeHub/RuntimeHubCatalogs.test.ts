import { describe, expect, it } from "vitest";

import { validateCatalogDraft } from "./RuntimeHubCatalogs";

describe("Runtime Hub catalog drafts", () => {
  it("accepts HTTPS and loopback HTTP sources", () => {
    expect(validateCatalogDraft([
      { sourceId: "curated", endpoint: "https://models.example.test/search" },
      { sourceId: "local-fixture", endpoint: "http://127.0.0.1:9099/catalog" },
    ])).toBeNull();
  });

  it("rejects duplicate ids, credentialed URLs, and insecure remote HTTP", () => {
    expect(validateCatalogDraft([
      { sourceId: "same", endpoint: "https://one.example.test" },
      { sourceId: "same", endpoint: "https://two.example.test" },
    ])).toContain("duplicated");
    expect(validateCatalogDraft([{ sourceId: "secret", endpoint: "https://user:pass@example.test" }])).toContain("HTTPS");
    expect(validateCatalogDraft([{ sourceId: "remote", endpoint: "http://example.test" }])).toContain("HTTPS");
  });
});

