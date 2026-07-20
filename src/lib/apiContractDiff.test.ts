import { describe, expect, it } from "vitest";
import {
  breakingChangeCount,
  diffApiDocuments,
  draftClientImpactNotes,
  generateContractTestStub,
  generateMockResponses,
  isReleaseReady,
  parseClientImpactResponse,
  parseOpenApiDocument,
  parseYamlOrJson,
  runGeneratedContractTests,
  validateContractValue,
  type ApiContractDiffCallResult,
} from "./apiContractDiff";

const OLD_JSON = JSON.stringify({
  openapi: "3.0.0",
  info: { title: "Widgets API", version: "1.0.0" },
  paths: {
    "/widgets": {
      get: {
        operationId: "listWidgets",
        parameters: [{ name: "limit", in: "query", required: false, schema: { type: "integer" } }],
        responses: {
          "200": {
            content: {
              "application/json": {
                schema: {
                  type: "object",
                  properties: {
                    items: {
                      type: "array",
                      items: {
                        type: "object",
                        properties: {
                          id: { type: "string" },
                          name: { type: "string" },
                          status: { type: "string", enum: ["active", "retired"] },
                        },
                        required: ["id", "name"],
                      },
                    },
                  },
                },
              },
            },
          },
        },
      },
      post: {
        operationId: "createWidget",
        requestBody: {
          required: true,
          content: {
            "application/json": {
              schema: {
                type: "object",
                properties: { name: { type: "string" } },
                required: ["name"],
              },
            },
          },
        },
        responses: { "201": { content: { "application/json": { schema: { type: "object", properties: { id: { type: "string" } } } } } } },
      },
    },
    "/widgets/{id}": {
      delete: {
        operationId: "deleteWidget",
        parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }],
        responses: { "204": {} },
      },
    },
  },
});

// A "new" version that introduces one of every breaking-change kind this
// module classifies, plus a couple of non-breaking ones, on top of the doc
// above:
// - DELETE /widgets/{id} removed entirely (breaking: endpoint-removed)
// - GET /widgets/{id} added (non-breaking: endpoint-added)
// - GET /widgets: `limit` param became required (breaking), response item
//   field `name` removed (breaking), `status` enum lost "retired" (breaking)
//   and gained "archived" (non-breaking), a new optional field `tags` added
//   (non-breaking)
// - POST /widgets: request body gained a new REQUIRED field `sku` (breaking)
const NEW_JSON = JSON.stringify({
  openapi: "3.0.0",
  info: { title: "Widgets API", version: "2.0.0" },
  paths: {
    "/widgets": {
      get: {
        operationId: "listWidgets",
        parameters: [{ name: "limit", in: "query", required: true, schema: { type: "integer" } }],
        responses: {
          "200": {
            content: {
              "application/json": {
                schema: {
                  type: "object",
                  properties: {
                    items: {
                      type: "array",
                      items: {
                        type: "object",
                        properties: {
                          id: { type: "string" },
                          status: { type: "string", enum: ["active", "archived"] },
                          tags: { type: "array", items: { type: "string" } },
                        },
                        required: ["id"],
                      },
                    },
                  },
                },
              },
            },
          },
        },
      },
      post: {
        operationId: "createWidget",
        requestBody: {
          required: true,
          content: {
            "application/json": {
              schema: {
                type: "object",
                properties: { name: { type: "string" }, sku: { type: "string" } },
                required: ["name", "sku"],
              },
            },
          },
        },
        responses: { "201": { content: { "application/json": { schema: { type: "object", properties: { id: { type: "string" } } } } } } },
      },
    },
    "/widgets/{id}": {
      get: {
        operationId: "getWidget",
        parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }],
        responses: { "200": { content: { "application/json": { schema: { type: "object", properties: { id: { type: "string" } } } } } } },
      },
    },
  },
});

describe("parseYamlOrJson", () => {
  it("parses a plain JSON document unchanged", () => {
    const value = parseYamlOrJson('{"a": 1, "b": [1, 2, "x"]}');
    expect(value).toEqual({ a: 1, b: [1, 2, "x"] });
  });

  it("parses a minimal OpenAPI-shaped YAML document", () => {
    const yaml = [
      "openapi: 3.0.0",
      "info:",
      "  title: Widgets API",
      "  version: 1.0.0",
      "paths:",
      "  /widgets:",
      "    get:",
      "      operationId: listWidgets",
      "      parameters:",
      "        - name: limit",
      "          in: query",
      "          required: false",
      "          schema:",
      "            type: integer",
      "      responses:",
      "        '200':",
      "          content:",
      "            application/json:",
      "              schema:",
      "                type: object",
      "                properties:",
      "                  items:",
      "                    type: array",
      "",
    ].join("\n");
    const value = parseYamlOrJson(yaml) as Record<string, unknown>;
    expect(value.openapi).toBe("3.0.0");
    const info = value.info as Record<string, unknown>;
    expect(info.title).toBe("Widgets API");
    const paths = value.paths as Record<string, unknown>;
    const widgets = paths["/widgets"] as Record<string, unknown>;
    const get = widgets.get as Record<string, unknown>;
    expect(get.operationId).toBe("listWidgets");
    const parameters = get.parameters as Array<Record<string, unknown>>;
    expect(parameters).toHaveLength(1);
    expect(parameters[0].name).toBe("limit");
    expect(parameters[0].required).toBe(false);
    const schema = parameters[0].schema as Record<string, unknown>;
    expect(schema.type).toBe("integer");
  });

  it("parses flow-style enum lists", () => {
    const yaml = ["status:", "  enum: [active, retired, archived]"].join("\n");
    const value = parseYamlOrJson(yaml) as Record<string, unknown>;
    const status = value.status as Record<string, unknown>;
    expect(status.enum).toEqual(["active", "retired", "archived"]);
  });
});

describe("parseOpenApiDocument", () => {
  it("extracts operations, parameters, request/response schemas from JSON", () => {
    const doc = parseOpenApiDocument(OLD_JSON, "old.json");
    expect(doc.title).toBe("Widgets API");
    expect(doc.version).toBe("1.0.0");
    expect(doc.operations.map((op) => `${op.method} ${op.path}`)).toEqual([
      "GET /widgets",
      "POST /widgets",
      "DELETE /widgets/{id}",
    ]);
    const listOp = doc.operations.find((op) => op.operationId === "listWidgets");
    expect(listOp?.parameters).toEqual([{ name: "limit", in: "query", required: false, schema: { type: "integer" } }]);
    const createOp = doc.operations.find((op) => op.operationId === "createWidget");
    expect(createOp?.requestBodyRequired).toBe(true);
    expect(createOp?.requestBodySchema?.required).toEqual(["name"]);
  });

  it("throws a clear error on a non-OpenAPI document rather than diffing garbage", () => {
    expect(() => parseOpenApiDocument('{"hello": "world"}', "not-openapi.json")).toThrow(/openapi|swagger/i);
  });

  it("throws a clear error on unparsable text", () => {
    expect(() => parseOpenApiDocument("{ this is not json or yaml : : :", "broken.json")).toThrow();
  });
});

describe("diffApiDocuments", () => {
  const oldDoc = parseOpenApiDocument(OLD_JSON, "old.json");
  const newDoc = parseOpenApiDocument(NEW_JSON, "new.json");
  const changes = diffApiDocuments(oldDoc, newDoc);

  it("classifies a removed endpoint as breaking", () => {
    const change = changes.find((c) => c.kind === "endpoint-removed");
    expect(change).toBeTruthy();
    expect(change?.operationLabel).toBe("DELETE /widgets/{id}");
    expect(change?.severity).toBe("breaking");
  });

  it("classifies an added endpoint as non-breaking", () => {
    const change = changes.find((c) => c.kind === "endpoint-added");
    expect(change).toBeTruthy();
    expect(change?.operationLabel).toBe("GET /widgets/{id}");
    expect(change?.severity).toBe("non-breaking");
  });

  it("classifies a param newly becoming required as breaking", () => {
    const change = changes.find((c) => c.kind === "param-now-required" && c.detail.includes("limit"));
    expect(change?.severity).toBe("breaking");
  });

  it("classifies a new required request-body field as breaking", () => {
    const change = changes.find((c) => c.kind === "field-now-required" && c.detail.includes("sku"));
    expect(change?.severity).toBe("breaking");
  });

  it("classifies a removed response field as breaking", () => {
    const change = changes.find((c) => c.kind === "field-removed" && c.detail.includes("name"));
    expect(change?.severity).toBe("breaking");
  });

  it("classifies a removed enum value as breaking and an added one as non-breaking", () => {
    const removed = changes.find((c) => c.kind === "enum-value-removed");
    const added = changes.find((c) => c.kind === "enum-value-added");
    expect(removed?.severity).toBe("breaking");
    expect(removed?.detail).toMatch(/retired/);
    expect(added?.severity).toBe("non-breaking");
    expect(added?.detail).toMatch(/archived/);
  });

  it("classifies a new optional field as non-breaking", () => {
    const change = changes.find((c) => c.kind === "field-added" && c.detail.includes("tags"));
    expect(change?.severity).toBe("non-breaking");
  });

  it("computes an accurate breaking count and release-ready verdict", () => {
    const count = breakingChangeCount(changes);
    const contractTests = runGeneratedContractTests(newDoc);
    expect(count).toBeGreaterThan(0);
    expect(isReleaseReady(changes, contractTests)).toBe(false);
    expect(isReleaseReady(changes.filter((c) => c.severity === "non-breaking"), contractTests)).toBe(true);
    expect(isReleaseReady([])).toBe(false);
  });

  it("returns no changes at all for a document diffed against itself", () => {
    const identical = diffApiDocuments(oldDoc, parseOpenApiDocument(OLD_JSON, "old-again.json"));
    expect(identical).toHaveLength(0);
  });
});

describe("generateMockResponses", () => {
  it("generates one example per response schema, respecting enum/type/array shape", () => {
    const doc = parseOpenApiDocument(OLD_JSON, "old.json");
    const mocks = generateMockResponses(doc);
    const listMock = mocks.find((m) => m.operationLabel === "GET /widgets" && m.status === "200");
    expect(listMock).toBeTruthy();
    const example = listMock!.example as { items: Array<Record<string, unknown>> };
    expect(Array.isArray(example.items)).toBe(true);
    const item = example.items[0];
    expect(typeof item.id).toBe("string");
    expect(item.status).toBe("active"); // first enum value
  });
});

describe("generateContractTestStub", () => {
  it("renders complete runnable vitest cases with concrete request/response samples", () => {
    const doc = parseOpenApiDocument(NEW_JSON, "new.json");
    const stub = generateContractTestStub(doc);
    expect(stub).toContain("import { describe, expect, it } from 'vitest';");
    expect(stub).toContain("POST /widgets");
    expect(stub).toContain('"name": "string"');
    expect(stub).not.toMatch(/TODO|samplePayload:\s*Record<string, unknown>\s*=\s*\{\}/);
  });

  it("executes every generated schema-backed case and requires a non-empty clean report", () => {
    const doc = parseOpenApiDocument(NEW_JSON, "new.json");
    const report = runGeneratedContractTests(doc);
    expect(report.results.length).toBeGreaterThan(0);
    expect(report.failCount).toBe(0);
    expect(report.clean).toBe(true);
  });

  it("reports concrete schema violations instead of trusting a release-ready boolean", () => {
    const errors = validateContractValue(
      { name: "widget" },
      { type: "object", properties: { name: { type: "string" }, sku: { type: "string" } }, required: ["name", "sku"] },
      {},
    );
    expect(errors).toEqual(expect.arrayContaining([expect.stringMatching(/sku.*missing/i)]));
  });
});

describe("parseClientImpactResponse", () => {
  it("parses a well-formed JSON array reply", () => {
    const notes = parseClientImpactResponse('[{"id":"change-1","impact":"Clients break.","migration":"Update the client."}]');
    expect(notes).toEqual([{ changeId: "change-1", impact: "Clients break.", migration: "Update the client." }]);
  });

  it("falls back to a default migration string when omitted", () => {
    const notes = parseClientImpactResponse('[{"id":"change-1","impact":"Clients break."}]');
    expect(notes[0].migration).toMatch(/review the change manually/i);
  });

  it("returns an empty array for unparsable content", () => {
    expect(parseClientImpactResponse("not json at all")).toEqual([]);
  });
});

describe("draftClientImpactNotes", () => {
  const oldDoc = parseOpenApiDocument(OLD_JSON, "old.json");
  const newDoc = parseOpenApiDocument(NEW_JSON, "new.json");
  const changes = diffApiDocuments(oldDoc, newDoc);

  it("returns no notes and never calls the model when there are no breaking changes", async () => {
    const callModel = async (): Promise<ApiContractDiffCallResult> => ({ content: "[]", streamError: null });
    const notes = await draftClientImpactNotes(
      changes.filter((c) => c.severity === "non-breaking"),
      callModel,
    );
    expect(notes).toEqual([]);
  });

  it("drafts one note per breaking change from a well-formed model reply", async () => {
    const breaking = changes.filter((c) => c.severity === "breaking");
    const reply = JSON.stringify(
      breaking.map((c) => ({ id: c.id, impact: `Impact for ${c.id}`, migration: `Migrate for ${c.id}` })),
    );
    const callModel = async (): Promise<ApiContractDiffCallResult> => ({ content: reply, streamError: null });
    const notes = await draftClientImpactNotes(changes, callModel);
    expect(notes).toHaveLength(breaking.length);
    expect(notes.map((n) => n.changeId).sort()).toEqual(breaking.map((c) => c.id).sort());
  });

  it("surfaces a stream error rather than fabricating notes", async () => {
    const callModel = async (): Promise<ApiContractDiffCallResult> => ({ content: "", streamError: "local model unreachable" });
    await expect(draftClientImpactNotes(changes, callModel)).rejects.toThrow(/unreachable/);
  });

  it("throws when the model reply cannot be parsed into any notes", async () => {
    const callModel = async (): Promise<ApiContractDiffCallResult> => ({ content: "not json", streamError: null });
    await expect(draftClientImpactNotes(changes, callModel)).rejects.toThrow(/did not return/i);
  });
});
