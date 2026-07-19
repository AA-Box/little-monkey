import { describe, expect, it } from "vitest";
import { exactBrowserOrigin, isLoopbackBrowserUrl } from "./browserVerification";

describe("browser verification grants", () => {
  it("reduces a URL to one exact origin", () => {
    expect(exactBrowserOrigin("https://example.com:8443/a?b=1")).toBe("https://example.com:8443");
    expect(exactBrowserOrigin("http://localhost:3000/path")).toBe("http://localhost:3000");
  });

  it("rejects schemes and credentials that the worker will not accept", () => {
    expect(() => exactBrowserOrigin("file:///tmp/a")).toThrow(/http/);
    expect(() => exactBrowserOrigin("https://user:secret@example.com/")).toThrow(/credentials/);
  });

  it("requires a named loopback grant for local testing", () => {
    expect(isLoopbackBrowserUrl("http://127.0.0.1:1420")).toBe(true);
    expect(isLoopbackBrowserUrl("http://localhost:5173")).toBe(true);
    expect(isLoopbackBrowserUrl("https://example.com")).toBe(false);
  });
});
