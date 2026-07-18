import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LittleMonkeyApiError, LittleMonkeyClient } from "./client";

const fetchMock = vi.fn();

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: () => Promise.resolve(JSON.stringify(body)),
  };
}

describe("LittleMonkeyClient request shapes", () => {
  it("chat() sends a POST with the bearer header, JSON content type, and stream forced false", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ id: "chatcmpl-1" }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1", token: "lmk-abc" });

    await client.chat({ model: "qwen2.5-7b-instruct", messages: [{ role: "user", content: "hi" }] });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:1234/v1/chat/completions");
    expect(init.method).toBe("POST");
    expect(init.headers.Authorization).toBe("Bearer lmk-abc");
    expect(init.headers["Content-Type"]).toBe("application/json");
    expect(JSON.parse(init.body)).toEqual({
      model: "qwen2.5-7b-instruct",
      messages: [{ role: "user", content: "hi" }],
      stream: false,
    });
  });

  it("models() sends a GET with no body and the bearer header", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ object: "list", data: [] }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1", token: "lmk-abc" });

    await client.models();

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:1234/v1/models");
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(init.headers.Authorization).toBe("Bearer lmk-abc");
    expect(init.headers["Content-Type"]).toBeUndefined();
  });

  it("knowledgeQuery() posts to /v1/knowledge/query with the request body untouched", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ query_id: "q1" }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1", token: "lmk-abc" });

    await client.knowledgeQuery({ stack_id: "stack-1", query: "hello" });

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:1234/v1/knowledge/query");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body)).toEqual({ stack_id: "stack-1", query: "hello" });
  });

  it("workflowRunStatus() GETs /v1/workflows/runs/{id} with the id URL-encoded", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ status: "completed" }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1", token: "lmk-abc" });

    await client.workflowRunStatus("run/with space");

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:1234/v1/workflows/runs/run%2Fwith%20space");
    expect(init.method).toBe("GET");
  });

  it("artifactRead() GETs /v1/artifacts/{id} with the id URL-encoded", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ blob: { id: "a1", size: 3 }, content_base64: "YWJj" }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1", token: "lmk-abc" });

    await client.artifactRead("a1/../b1");

    const [url] = fetchMock.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:1234/v1/artifacts/a1%2F..%2Fb1");
  });

  it("health() hits the server root, not the /v1 prefix, and needs no auth header to be sent by the caller", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ status: "ok" }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1" });

    await client.health();

    const [url] = fetchMock.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:1234/health");
  });

  it("omits the Authorization header entirely when no token is configured", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ object: "list", data: [] }));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1" });

    await client.models();

    const [, init] = fetchMock.mock.calls[0];
    expect(init.headers.Authorization).toBeUndefined();
  });

  it("throws LittleMonkeyApiError carrying the status and parsed body on a non-2xx response", async () => {
    fetchMock.mockResolvedValue(jsonResponse({ error: { message: "isn't scoped for `chat`" } }, 403));
    const client = new LittleMonkeyClient({ baseUrl: "http://127.0.0.1:1234/v1", token: "lmk-models-only" });

    await expect(
      client.chat({ model: "x", messages: [{ role: "user", content: "hi" }] }),
    ).rejects.toMatchObject({
      status: 403,
      body: { error: { message: "isn't scoped for `chat`" } },
    });
    await expect(
      client.chat({ model: "x", messages: [{ role: "user", content: "hi" }] }),
    ).rejects.toBeInstanceOf(LittleMonkeyApiError);
  });
});
