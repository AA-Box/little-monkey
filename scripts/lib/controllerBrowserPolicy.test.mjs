/**
 * The paired-device controller's response policy, enforced by a real browser.
 *
 * Run with: pnpm test:browser-policy
 *
 * **What this proves:** that the headers the runner really sends do not forbid
 * the browser APIs the controller really calls. A permissions policy with an
 * empty allowlist — `camera=()` — disables the feature for the document, and a
 * CSP without `media-src` falls back to `default-src 'none'`. Both were true of
 * every response this runner sent, so `getUserMedia`, `getDisplayMedia`,
 * `getCurrentPosition` and every audio element were refused before any
 * permission prompt could appear. No jsdom test can see that: jsdom enforces
 * neither header, and the client's own modules never meet one.
 *
 * **What this does NOT prove:** that a camera, a microphone or a GPS works.
 * Nothing here opens a device — a headless browser has none to open, and a
 * request for one is answered by the browser's own permission layer, which this
 * deliberately never reaches. Real hardware stays a manual smoke test on a real
 * paired phone: pair it, press each preparation control, run one command per
 * capability.
 *
 * The page is served from a throwaway HTTP server on `127.0.0.1` — a secure
 * context, so nothing here is refused for the *lack* of one — with the header
 * constants parsed out of `daemon/remote/web.rs`. Parsed rather than restated,
 * so a policy edit that breaks the client fails here instead of shipping.
 *
 * A negative control runs in the same session: the same page under the headers
 * this runner used to send must report every one of those features disabled and
 * both audio sources blocked. A test that cannot fail proves nothing.
 */
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { test } from "node:test";

const REMOTE = join(
  dirname(fileURLToPath(import.meta.url)),
  "../../src-tauri/src/bin/monkey-cli/daemon/remote",
);

/** The header set every response carried before the controller got its own. */
const PREVIOUS_PERMISSIONS_POLICY =
  "camera=(), microphone=(), geolocation=(), display-capture=(), payment=(), usb=()";
const PREVIOUS_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; " +
  "img-src 'self' data:; manifest-src 'self'; base-uri 'none'; form-action 'none'; " +
  "frame-ancestors 'none'; object-src 'none'";

const FEATURES = ["camera", "microphone", "geolocation", "display-capture"];

const ASSETS = {
  "/remote": ["text/html; charset=utf-8", "ui/index.html"],
  "/v1/remote/ui/app.css": ["text/css; charset=utf-8", "ui/app.css"],
  "/v1/remote/ui/app.js": ["text/javascript; charset=utf-8", "ui/app.js"],
  "/v1/remote/ui/device-core.js": ["text/javascript; charset=utf-8", "ui/device-core.js"],
  "/v1/remote/ui/manifest.webmanifest": [
    "application/manifest+json; charset=utf-8",
    "ui/manifest.webmanifest",
  ],
  "/v1/remote/ui/icon.svg": ["image/svg+xml; charset=utf-8", "ui/icon.svg"],
};

/**
 * One `pub const NAME: &str = "…";` from `web.rs`, with Rust's line
 * continuations resolved the way rustc resolves them: a trailing backslash eats
 * the newline and the indentation that follows it.
 */
export function rustStringConstant(source, name) {
  const declaration = source.indexOf(`pub const ${name}: &str =`);
  assert.notEqual(declaration, -1, `web.rs no longer declares ${name}`);
  const open = source.indexOf('"', declaration);
  let value = "";
  for (let index = open + 1; index < source.length; index += 1) {
    const character = source[index];
    if (character === '"') return value;
    if (character === "\\" && source[index + 1] === "\n") {
      index += 1;
      while (/\s/u.test(source[index + 1])) index += 1;
      continue;
    }
    value += character;
  }
  throw new Error(`${name} is not a closed string literal`);
}

function policies() {
  const source = readFileSync(join(REMOTE, "web.rs"), "utf8");
  return {
    controllerPermissions: rustStringConstant(source, "CONTROLLER_PERMISSIONS_POLICY"),
    controllerCsp: rustStringConstant(source, "CONTROLLER_CSP"),
    apiPermissions: rustStringConstant(source, "API_PERMISSIONS_POLICY"),
    apiCsp: rustStringConstant(source, "API_CSP"),
  };
}

/**
 * Serves the controller's real files with the runner's real headers, plus the
 * same document under the previous ones at `/legacy` for the negative control.
 */
function serveController(policy) {
  const server = createServer((request, response) => {
    const path = request.url.split("?")[0];
    const legacy = path === "/legacy";
    const asset = ASSETS[legacy ? "/remote" : path];
    if (!asset) {
      response.writeHead(404, { "content-type": "application/json" });
      response.end("{}");
      return;
    }
    const [contentType, file] = asset;
    const document = contentType.startsWith("text/html");
    response.writeHead(200, {
      "content-type": contentType,
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
      "referrer-policy": "no-referrer",
      "permissions-policy": legacy
        ? PREVIOUS_PERMISSIONS_POLICY
        : document
          ? policy.controllerPermissions
          : policy.apiPermissions,
      "content-security-policy": legacy
        ? PREVIOUS_CSP
        : document
          ? policy.controllerCsp
          : policy.apiCsp,
    });
    response.end(readFileSync(join(REMOTE, file)));
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, origin: `http://127.0.0.1:${server.address().port}` }),
    );
  });
}

/** A browser engine this machine already has. Never downloads one. */
export function findBrowser(env = process.env, exists = existsSync) {
  for (const named of [env.PUPPETEER_EXECUTABLE_PATH, env.CHROME_PATH]) {
    if (named && exists(named)) return named;
  }
  const candidates = {
    linux: [
      "/usr/bin/google-chrome",
      "/usr/bin/google-chrome-stable",
      "/usr/bin/chromium-browser",
      "/usr/bin/chromium",
    ],
    darwin: [
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ],
    win32: [
      "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
      "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    ],
  };
  return (candidates[process.platform] || []).find((path) => exists(path)) || null;
}

/**
 * What the page reports about itself: which features its policy permits, and
 * whether the two audio sources the controller really loads are allowed to.
 */
const INSPECT = `(async () => {
  const policy = document.permissionsPolicy || document.featurePolicy;
  if (!policy) return { error: "this browser exposes no permissions policy API" };
  const allowed = {};
  for (const feature of ["camera", "microphone", "geolocation", "display-capture", "payment", "usb"]) {
    try { allowed[feature] = policy.allowsFeature(feature); } catch { allowed[feature] = null; }
  }
  const violations = [];
  document.addEventListener("securitypolicyviolation", (event) => violations.push(event.violatedDirective));
  const load = (url) => new Promise((resolve) => {
    const audio = new Audio(url);
    audio.addEventListener("loadedmetadata", () => resolve(true));
    audio.addEventListener("error", () => resolve(false));
    setTimeout(() => resolve(false), 2000);
  });
  const silence = "data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEAgD4AAAB9AAACABAAZGF0YQAAAAA=";
  const bytes = Uint8Array.from(atob(silence.split(",")[1]), (character) => character.charCodeAt(0));
  const dataAudio = await load(silence);
  const blobAudio = await load(URL.createObjectURL(new Blob([bytes], { type: "audio/wav" })));
  return { allowed, dataAudio, blobAudio, violations, secureContext: isSecureContext };
})()`;

test("the served controller policy permits every browser API the client calls", async (t) => {
  const required = process.env.REQUIRE_BROWSER === "1";
  let puppeteer;
  try {
    puppeteer = (await import("puppeteer-core")).default;
  } catch {
    const reason = "puppeteer-core is not installed";
    assert.ok(!required, `REQUIRE_BROWSER=1 and ${reason}`);
    t.skip(`SKIPPED: ${reason}`);
    return;
  }
  const executablePath = findBrowser();
  if (!executablePath) {
    const reason = "no Chrome or Chromium on this machine (set PUPPETEER_EXECUTABLE_PATH)";
    assert.ok(!required, `REQUIRE_BROWSER=1 and ${reason}`);
    t.skip(`SKIPPED: ${reason}`);
    return;
  }

  const policy = policies();
  const { server, origin } = await serveController(policy);
  const browser = await puppeteer.launch({
    executablePath,
    headless: true,
    // Hosted runners have no user namespaces to sandbox into. Nothing untrusted
    // is loaded here: the page comes from this repository's own files.
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  try {
    const inspect = async (path) => {
      const page = await browser.newPage();
      try {
        await page.goto(`${origin}${path}`, { waitUntil: "domcontentloaded" });
        return await page.evaluate(INSPECT);
      } finally {
        await page.close();
      }
    };

    const controller = await inspect("/remote");
    assert.equal(controller.error, undefined, controller.error);
    assert.equal(controller.secureContext, true, "127.0.0.1 must be a secure context");
    for (const feature of FEATURES) {
      assert.equal(
        controller.allowed[feature],
        true,
        `the controller calls ${feature} and the served policy forbids it`,
      );
    }
    // Narrow, not open: what the controller does not use stays denied.
    for (const denied of ["payment", "usb"]) {
      assert.equal(controller.allowed[denied], false, `${denied} must stay denied`);
    }
    // The two audio sources the controller really loads: an artifact's `blob:`
    // URL, and the `data:` silence that unlocks autoplay.
    assert.equal(controller.dataAudio, true, "the autoplay-unlocking silence must load");
    assert.equal(controller.blobAudio, true, "a played artifact's blob: URL must load");
    assert.deepEqual(controller.violations, [], "the controller's own page must violate nothing");

    // The negative control. Without it this test could pass against a browser
    // that enforces no policy at all.
    const previous = await inspect("/legacy");
    for (const feature of FEATURES) {
      assert.equal(
        previous.allowed[feature],
        false,
        `the previous headers must be observably worse, and ${feature} was permitted`,
      );
    }
    assert.equal(previous.dataAudio, false, "the previous CSP must block data: audio");
    assert.equal(previous.blobAudio, false, "the previous CSP must block blob: audio");
  } finally {
    await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
