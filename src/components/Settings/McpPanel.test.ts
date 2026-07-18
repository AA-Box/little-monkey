import { describe, expect, it } from "vitest";

import { APP_CONNECTOR_TEMPLATES } from "./McpPanel";

describe("MCP connector templates", () => {
  it("keeps the GitHub connector read-only until the user explicitly changes it", () => {
    const github = APP_CONNECTOR_TEMPLATES.find((template) => template.id === "github");

    expect(github?.draft.transportKind).toBe("stdio");
    expect(github?.draft.env).toMatchObject({
      GITHUB_READ_ONLY: "1",
      GITHUB_TOOLSETS: "repos,issues,pull_requests,actions",
    });
    expect(github?.draft.argsText?.split("\n")).toContain("GITHUB_READ_ONLY");
  });

  it("cannot submit the custom HTTP template before the user supplies a real endpoint", () => {
    const custom = APP_CONNECTOR_TEMPLATES.find((template) => template.id === "custom-http");

    expect(custom?.draft.transportKind).toBe("http");
    expect(custom?.draft.url).toBe("");
  });

  it("uses stable unique ids for every template", () => {
    const ids = APP_CONNECTOR_TEMPLATES.map((template) => template.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("includes a Google Drive template pointed at the real hosted MCP endpoint", () => {
    const googleDrive = APP_CONNECTOR_TEMPLATES.find((template) => template.id === "google-drive");

    expect(googleDrive?.draft.transportKind).toBe("http");
    expect(googleDrive?.draft.url).toBe("https://drivemcp.googleapis.com/mcp/v1");
  });

  it("includes a Gmail template pointed at the real hosted MCP endpoint", () => {
    const gmail = APP_CONNECTOR_TEMPLATES.find((template) => template.id === "gmail");

    expect(gmail?.draft.transportKind).toBe("http");
    expect(gmail?.draft.url).toBe("https://gmailmcp.googleapis.com/mcp/v1");
  });
});
