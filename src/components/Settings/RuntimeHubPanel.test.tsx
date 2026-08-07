import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { RuntimeHubPanel } from "./RuntimeHubPanel";

describe("Runtime Hub section tabs", () => {
  it("renders one accessible non-wrapping row with narrow-width overflow", () => {
    const markup = renderToStaticMarkup(<RuntimeHubPanel />);
    const tablist = markup.match(/<div role="tablist"[^>]*>/)?.[0];

    expect(tablist).toContain('aria-orientation="horizontal"');
    expect(tablist).toContain("flex-nowrap");
    expect(tablist).toContain("overflow-x-auto");
    expect(tablist).not.toContain("grid-cols");
    expect(markup.match(/role="tab"/g)).toHaveLength(13);
    expect(markup.match(/role="tab"[^>]*tabindex="0"/g)).toHaveLength(1);
    expect(markup.match(/role="tab"[^>]*tabindex="-1"/g)).toHaveLength(12);
  });
});
