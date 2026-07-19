import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  attemptStream: vi.fn(),
  resolveTarget: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: () => false,
}));
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));
vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
}));

import { validateServerSpec } from "./mcpGenerator";
import { generateFixtures, runSimulation } from "./mcpSimulator";
import {
  buildConnectorDefinition,
  buildDraftPrompt,
  detectSpecFormat,
  draftConnectorSummary,
  parseOpenApiDocument,
  parseOpenApiSpec,
  parseYamlSubset,
  resolveConnectorDraftTarget,
} from "./connectorBuilder";

beforeEach(() => {
  mocks.attemptStream.mockReset();
  mocks.resolveTarget.mockReset();
});

const OPENAPI_JSON = {
  openapi: "3.0.0",
  info: { title: "Pet Store API", version: "1.2.0" },
  servers: [{ url: "https://api.petstore.example.com/v1" }],
  security: [{ bearerAuth: [] }],
  components: {
    securitySchemes: {
      bearerAuth: { type: "http", scheme: "bearer" },
    },
  },
  paths: {
    "/pets": {
      get: {
        operationId: "listPets",
        summary: "List all pets",
        parameters: [
          { name: "limit", in: "query", required: false, schema: { type: "integer" } },
        ],
        security: [],
      },
      post: {
        operationId: "createPet",
        summary: "Create a pet",
        requestBody: { required: true, description: "The pet to create." },
      },
    },
    "/pets/{petId}": {
      parameters: [{ name: "petId", in: "path", required: true, schema: { type: "string" } }],
      get: {
        operationId: "getPetById",
        summary: "Get a pet by id",
        security: [],
      },
      delete: {
        operationId: "deletePet",
        summary: "Delete a pet",
      },
    },
  },
};

describe("detectSpecFormat", () => {
  it("detects JSON from a leading brace", () => {
    expect(detectSpecFormat('{"a": 1}')).toBe("json");
  });

  it("detects YAML by default for non-brace content", () => {
    expect(detectSpecFormat("openapi: 3.0.0")).toBe("yaml");
  });

  it("prefers the file extension when given", () => {
    expect(detectSpecFormat('{"a": 1}', "spec.yaml")).toBe("yaml");
    expect(detectSpecFormat("openapi: 3.0.0", "spec.json")).toBe("json");
  });
});

describe("parseYamlSubset", () => {
  it("parses nested mappings and scalars", () => {
    const yaml = [
      "openapi: 3.0.0",
      "info:",
      "  title: Widgets API",
      "  version: '2.0'",
      "count: 3",
      "flag: true",
    ].join("\n");
    expect(parseYamlSubset(yaml)).toEqual({
      openapi: "3.0.0",
      info: { title: "Widgets API", version: "2.0" },
      count: 3,
      flag: true,
    });
  });

  it("parses a sequence of mappings (e.g. OpenAPI parameters)", () => {
    const yaml = [
      "parameters:",
      "  - name: id",
      "    in: path",
      "    required: true",
      "  - name: verbose",
      "    in: query",
      "    required: false",
    ].join("\n");
    expect(parseYamlSubset(yaml)).toEqual({
      parameters: [
        { name: "id", in: "path", required: true },
        { name: "verbose", in: "query", required: false },
      ],
    });
  });

  it("ignores comments and blank lines", () => {
    const yaml = ["# a comment", "a: 1", "", "b: 2 # trailing comment"].join("\n");
    expect(parseYamlSubset(yaml)).toEqual({ a: 1, b: 2 });
  });
});

describe("parseOpenApiDocument", () => {
  it("parses well-formed JSON", () => {
    const doc = parseOpenApiDocument(JSON.stringify(OPENAPI_JSON));
    expect((doc.info as { title: string }).title).toBe("Pet Store API");
  });

  it("parses an equivalent YAML document to the same shape", () => {
    const yaml = [
      "openapi: 3.0.0",
      "info:",
      "  title: Pet Store API",
      "  version: 1.2.0",
      "servers:",
      "  - url: https://api.petstore.example.com/v1",
      "paths:",
      "  /pets:",
      "    get:",
      "      operationId: listPets",
      "      summary: List all pets",
      "      parameters:",
      "        - name: limit",
      "          in: query",
      "          required: false",
      "          schema:",
      "            type: integer",
    ].join("\n");
    const doc = parseOpenApiDocument(yaml, "petstore.yaml");
    expect((doc.info as { title: string }).title).toBe("Pet Store API");
    expect((doc.paths as Record<string, unknown>)["/pets"]).toBeTruthy();
  });

  it("throws a clear error on unparseable input", () => {
    expect(() => parseOpenApiDocument("{ not: valid ] json", "spec.json")).toThrow(/Could not parse/);
  });
});

describe("parseOpenApiSpec + buildConnectorDefinition", () => {
  it("extracts operations, params, auth, and rate-limit hints", () => {
    const parsed = parseOpenApiSpec(JSON.stringify(OPENAPI_JSON));
    expect(parsed.title).toBe("Pet Store API");
    expect(parsed.baseUrl).toBe("https://api.petstore.example.com/v1");
    expect(parsed.auth.type).toBe("httpBearer");
    expect(parsed.rateLimit.declared).toBe(false);
    expect(parsed.operations).toHaveLength(4);

    const listPets = parsed.operations.find((op) => op.method === "GET" && op.path === "/pets");
    expect(listPets?.requiresAuth).toBe(false); // per-operation `security: []` override
    expect(listPets?.params).toEqual([{ name: "limit", type: "number", required: false, description: "(query)" }]);

    const createPet = parsed.operations.find((op) => op.method === "POST");
    expect(createPet?.requiresAuth).toBe(true); // inherits global security
    expect(createPet?.params).toEqual([
      { name: "body", type: "object", required: true, description: "The pet to create." },
    ]);

    const getPetById = parsed.operations.find((op) => op.method === "GET" && op.path === "/pets/{petId}");
    expect(getPetById?.params).toEqual([{ name: "petId", type: "string", required: true, description: "(path)" }]);
  });

  it("builds an McpServerSpec that validateServerSpec accepts and mcpSimulator can run against", () => {
    const parsed = parseOpenApiSpec(JSON.stringify(OPENAPI_JSON));
    const definition = buildConnectorDefinition(parsed);

    expect(validateServerSpec(definition.server)).toEqual([]);
    expect(definition.server.name).toBe("pet-store-api");
    expect(definition.server.sourceKind).toBe("api");
    expect(definition.server.tools.map((tool) => tool.name)).toEqual([
      "list_pets",
      "create_pet",
      "get_pet_by_id",
      "delete_pet",
    ]);

    expect(definition.permissions).toHaveLength(4);
    expect(definition.permissions.find((p) => p.toolName === "delete_pet")?.risk).toBe("high");
    expect(definition.permissions.find((p) => p.toolName === "list_pets")?.risk).toBe("low");
    expect(definition.permissions.find((p) => p.toolName === "create_pet")?.risk).toBe("medium");

    // The simulator (reused, not duplicated) can run against the generated spec.
    const fixtures = generateFixtures(definition.server);
    expect(fixtures.length).toBeGreaterThan(0);
    const report = runSimulation(definition.server);
    expect(report.clean).toBe(true);
  });

  it("dedupes tool names that would otherwise collide after sanitization", () => {
    const collidingSpec = {
      openapi: "3.0.0",
      info: { title: "Collisions" },
      paths: {
        "/a": { get: { operationId: "get-thing" } },
        "/b": { get: { operationId: "get_thing" } },
      },
    };
    const parsed = parseOpenApiSpec(JSON.stringify(collidingSpec));
    const definition = buildConnectorDefinition(parsed);
    const names = definition.server.tools.map((tool) => tool.name);
    expect(new Set(names).size).toBe(names.length);
    expect(validateServerSpec(definition.server)).toEqual([]);
  });

  it("throws when the spec has no paths", () => {
    expect(() => parseOpenApiSpec(JSON.stringify({ openapi: "3.0.0", info: { title: "Empty" } }))).toThrow(/paths/);
  });

  it("falls back to Swagger 2.0 host/basePath/schemes for the base URL", () => {
    const swagger2 = {
      swagger: "2.0",
      info: { title: "Legacy API", version: "1.0" },
      host: "legacy.example.com",
      basePath: "/v1",
      schemes: ["https"],
      paths: { "/items": { get: { operationId: "listItems" } } },
    };
    const parsed = parseOpenApiSpec(JSON.stringify(swagger2));
    expect(parsed.baseUrl).toBe("https://legacy.example.com/v1");
  });

  it("marks apiKey security schemes with the declared header/query location", () => {
    const spec = {
      openapi: "3.0.0",
      info: { title: "Keyed API" },
      security: [{ apiKeyAuth: [] }],
      components: { securitySchemes: { apiKeyAuth: { type: "apiKey", name: "X-Api-Key", in: "header" } } },
      paths: { "/data": { get: { operationId: "getData" } } },
    };
    const parsed = parseOpenApiSpec(JSON.stringify(spec));
    expect(parsed.auth).toEqual({
      type: "apiKey",
      paramName: "X-Api-Key",
      in: "header",
      instructions: 'Send the API key in the "X-Api-Key" header.',
    });
  });
});

describe("draftConnectorSummary", () => {
  it("returns the model's plain-text paragraph", async () => {
    const parsed = parseOpenApiSpec(JSON.stringify(OPENAPI_JSON));
    const definition = buildConnectorDefinition(parsed);
    mocks.attemptStream.mockResolvedValue({ content: "A tidy connector for pet store operations.", toolCalls: [], streamError: null });

    const target = { kind: "test" } as never;
    const summary = await draftConnectorSummary(definition, target);
    expect(summary).toBe("A tidy connector for pet store operations.");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
  });

  it("throws on a stream error", async () => {
    const parsed = parseOpenApiSpec(JSON.stringify(OPENAPI_JSON));
    const definition = buildConnectorDefinition(parsed);
    mocks.attemptStream.mockResolvedValue({ content: "", toolCalls: [], streamError: "boom" });

    await expect(draftConnectorSummary(definition, {} as never)).rejects.toThrow("boom");
  });

  it("throws when the model returns a tool call instead of text", async () => {
    const parsed = parseOpenApiSpec(JSON.stringify(OPENAPI_JSON));
    const definition = buildConnectorDefinition(parsed);
    mocks.attemptStream.mockResolvedValue({ content: "", toolCalls: [{ id: "1" }], streamError: null });

    await expect(draftConnectorSummary(definition, {} as never)).rejects.toThrow(/tool call/);
  });

  it("builds a prompt carrying the connector's tools and auth type", () => {
    const parsed = parseOpenApiSpec(JSON.stringify(OPENAPI_JSON));
    const definition = buildConnectorDefinition(parsed);
    const { system, user } = buildDraftPrompt(definition);
    expect(system).toMatch(/short, clear paragraph/);
    expect(JSON.parse(user).authType).toBe("httpBearer");
    expect(JSON.parse(user).toolCount).toBe(4);
  });
});

describe("resolveConnectorDraftTarget", () => {
  it("delegates to agentLoop's resolveTarget", async () => {
    mocks.resolveTarget.mockResolvedValue({ kind: "resolved" });
    const target = await resolveConnectorDraftTarget();
    expect(target).toEqual({ kind: "resolved" });
    expect(mocks.resolveTarget).toHaveBeenCalledTimes(1);
  });
});
