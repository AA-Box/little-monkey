#!/usr/bin/env node
/**
 * A complete Studio tool, in as little code as the contract allows.
 *
 * It does nothing useful on purpose: it hands back the image it was given, with
 * the other inputs echoed into its log. What it demonstrates is the whole tier —
 * launch, manifest, form, run, gallery — so you can see a tool work before
 * writing one that does something.
 *
 * See `docs/studio-tool-contract.md`. No dependencies; Node's own http is all a
 * tool needs.
 *
 *   node examples/studio-tool-echo.mjs --host 127.0.0.1 --port 9099
 */
import { createServer } from "node:http";

// The app always passes both. Defaults are here so the script is runnable by
// hand, which is how you would test one of these while writing it.
const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
};
const host = flag("host", "127.0.0.1");
const port = Number(flag("port", "9099"));

/** This is the tool's entire user interface. Studio renders a card per input. */
const MANIFEST = {
  schemaVersion: 1,
  id: "echo",
  name: "Echo",
  description:
    "Returns the image you give it, unchanged. A reference tool for the Studio tool contract.",
  inputs: [
    {
      key: "image",
      label: "Image",
      kind: "image",
      required: true,
      hint: "Handed straight back. Nothing is done to it.",
    },
    {
      key: "note",
      label: "Note",
      kind: "text",
      hint: "Echoed to this tool's log, to show text reaching the tool.",
    },
    {
      key: "amount",
      label: "Amount",
      kind: "number",
      min: 0,
      max: 10,
      step: 0.5,
      default: 1,
    },
    {
      key: "mode",
      label: "Mode",
      kind: "choice",
      default: "passthrough",
      options: [
        { value: "passthrough", label: "Pass through" },
        { value: "fail", label: "Fail on purpose" },
      ],
    },
    { key: "loud", label: "Verbose log", kind: "toggle", default: false },
  ],
};

/** Bodies are bounded here too. A tool is the last thing that should fall over
 *  because something upstream sent it more than it expected. */
const MAX_BODY_BYTES = 64 * 1024 * 1024;

function readBody(request) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    request.on("data", (chunk) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        reject(new Error("request body is too large"));
        request.destroy();
        return;
      }
      chunks.push(chunk);
    });
    request.on("end", () => resolve(Buffer.concat(chunks)));
    request.on("error", reject);
  });
}

function send(response, status, payload) {
  const body = JSON.stringify(payload);
  response.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
  });
  response.end(body);
}

const server = createServer(async (request, response) => {
  if (request.method === "GET" && request.url === "/tool/v1/manifest") {
    send(response, 200, MANIFEST);
    return;
  }

  if (request.method === "POST" && request.url === "/tool/v1/run") {
    let inputs;
    try {
      ({ inputs } = JSON.parse((await readBody(request)).toString("utf8")));
    } catch (cause) {
      // The app shows this sentence verbatim, so it has to say what is wrong.
      send(response, 400, { error: `could not read the run request: ${cause.message}` });
      return;
    }

    if (inputs?.loud) console.error(`run: ${JSON.stringify({ ...inputs, image: "<image>" })}`);

    // The failure path is worth exercising: pick "Fail on purpose" in Studio and
    // this sentence is what appears in the panel.
    if (inputs?.mode === "fail") {
      send(response, 400, { error: "asked to fail on purpose, so here is a failure" });
      return;
    }

    if (typeof inputs?.image !== "string" || inputs.image.length === 0) {
      send(response, 400, { error: "no image was supplied" });
      return;
    }

    send(response, 200, {
      media: [{ mediaType: "image/png", dataBase64: inputs.image }],
    });
    return;
  }

  send(response, 404, { error: "unknown route" });
});

// Logged to stderr because the app drains both streams and shows their tail
// when a tool fails to start — a tool that starts silently is a tool nobody can
// diagnose.
server.listen(port, host, () => console.error(`echo tool listening on ${host}:${port}`));
