/**
 * The Telephony bridge contract: what the panel sends is what the daemon's
 * commands take, and what the daemon serves is where the panel points a
 * carrier.
 *
 * These are fixed-argument Tauri commands, so a mismatch is not a type error
 * anywhere — `invoke` takes `unknown`, and a renamed argument simply arrives as
 * `undefined`. On this surface that failure is expensive and silent: a number
 * that quietly never saves its callback URL looks exactly like a carrier
 * outage, and the callback path itself is what Twilio and Plivo sign, so a
 * frontend that publishes a different one than the daemon rebuilds rejects
 * every genuine callback.
 *
 * Only names, registration and the shared path are checked here. Whether a
 * policy is legal and what a carrier is allowed to do stay the daemon's
 * answers; the frontend must not hold a second copy of those rules.
 */
import { readFileSync } from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import {
  CARRIER_GUIDES,
  callbackPath,
  callbackUrl,
  statusCallbackPath,
  statusCallbackUrl,
  type TelecomAccount,
} from "./telecomClient";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../");

const read = (relative: string) => readFileSync(path.join(REPO_ROOT, relative), "utf8");

const daemonCommands = read("src-tauri/src/daemon_commands.rs");
const libRs = read("src-tauri/src/lib.rs");
const telecomClient = read("src/lib/telecomClient.ts");
const telecomCli = read("src-tauri/src/bin/monkey-cli/telecom_cli.rs");
const telephonyMod = read("src-tauri/src/bin/monkey-cli/daemon/telephony/mod.rs");
const telephonyPanel = read("src/components/Settings/TelephonyPanel.tsx");

describe("the telephony bridge", () => {
  it("registers every command the panel invokes", () => {
    const invoked = [
      ...new Set(
        [...telecomClient.matchAll(/invoke<[^>]*>\(\s*"(telecom_\w+)"/g)].map((match) => match[1]),
      ),
    ];
    // A guard against the regex silently matching nothing and the rest of this
    // test passing vacuously.
    expect(invoked.length).toBeGreaterThan(8);
    for (const command of invoked) {
      expect(daemonCommands, `${command} has no #[tauri::command]`).toContain(
        `pub async fn ${command}(`,
      );
      expect(libRs, `${command} is not in generate_handler!`).toContain(
        `daemon_commands::${command},`,
      );
    }
  });

  it("covers every operation the panel needs, in both directions", () => {
    // Create, read, update, control, status and delete. A gap here is an
    // operator who has to open a terminal to finish setting up their own
    // phone number.
    for (const command of [
      "telecom_add",
      "telecom_list",
      "telecom_calls",
      "telecom_messages",
      "telecom_callback_url",
      "telecom_set_public_url",
      "telecom_set_credential",
      "telecom_set_policy",
      "telecom_set_limits",
      "telecom_set_greeting",
      "telecom_probe",
      "telecom_enable",
      "telecom_remove",
    ]) {
      expect(telecomClient, `${command} is not reachable from the panel`).toContain(`"${command}"`);
      expect(daemonCommands).toContain(`pub async fn ${command}(`);
    }
  });

  it("names the same arguments the daemon commands declare", () => {
    // `invoke` passes camelCase names that Tauri maps to the snake_case
    // parameters below. A rename on either side arrives as `undefined`, which
    // is why the two are compared rather than trusted.
    const expected: Record<string, string[]> = {
      telecom_messages: ["account_id", "limit"],
      telecom_set_public_url: ["account_id", "url", "config"],
      telecom_calls: ["account_id", "limit"],
      telecom_set_policy: ["account_id", "inbound", "outbound"],
      telecom_set_limits: [
        "account_id",
        "max_concurrent",
        "ring_timeout_s",
        "max_duration_s",
        "recording",
      ],
    };
    for (const [command, parameters] of Object.entries(expected)) {
      const start = daemonCommands.indexOf(`pub async fn ${command}(`);
      expect(start, `${command} not found`).toBeGreaterThan(-1);
      const signature = daemonCommands.slice(start, daemonCommands.indexOf(") ->", start));
      for (const parameter of parameters) {
        expect(signature, `${command} is missing ${parameter}`).toContain(`${parameter}:`);
      }
    }
  });

  it("publishes the same callback path the daemon serves and signs", () => {
    // Three places have to agree exactly: this function, the listener's route,
    // and the URL the Twilio and Plivo verifiers rebuild to check a signature.
    // The Rust side derives all three from one function, so comparing against
    // that literal is comparing against all of them.
    const rust = telephonyMod.match(/format!\("\/v1\/telecom\/\{account_id\}"\)/);
    expect(rust, "the daemon's callback_path changed shape").not.toBeNull();
    expect(callbackPath("tel-1")).toBe("/v1/telecom/tel-1");
  });

  it("publishes the status path the daemon serves separately", () => {
    // The two paths mean opposite things — a question the daemon answers with
    // stream markup, and a report it acknowledges — so a UI that published one
    // for the other would have a carrier connecting calls that already ended.
    const rust = telephonyMod.match(/format!\("\{\}\/status", callback_path\(account_id\)\)/);
    expect(rust, "the daemon's status_callback_path changed shape").not.toBeNull();
    expect(statusCallbackPath("tel-1")).toBe("/v1/telecom/tel-1/status");
    const account = {
      account_id: "tel-1",
      public_base_url: "https://calls.example.test/",
    } as TelecomAccount;
    expect(statusCallbackUrl(account)).toBe("https://calls.example.test/v1/telecom/tel-1/status");
    expect(statusCallbackUrl({ ...account, public_base_url: null })).toBeNull();
  });

  it("builds a copyable callback URL only from what the daemon stored", () => {
    const account = {
      account_id: "tel-1",
      public_base_url: "https://calls.example.test/",
    } as TelecomAccount;

    expect(callbackUrl(account)).toBe("https://calls.example.test/v1/telecom/tel-1");
    expect(callbackUrl({ ...account, public_base_url: null })).toBeNull();
  });

  it("never composes a callback host itself", () => {
    // The daemon is the only authority on what it is reachable as. A frontend
    // that glues `window.location.origin` onto a path hands the operator a URL
    // their carrier will post to and nothing will answer.
    for (const source of [telecomClient, telephonyPanel]) {
      expect(source).not.toMatch(/window\.location/);
    }
  });

  it("never talks to a carrier from the frontend", () => {
    // Every telephony call is a daemon command. A request issued here would be
    // the app talking to Twilio directly, outside the daemon's egress policy,
    // its credential handling and its durable event log. The carrier URLs this
    // file does hold are documentation links, opened in a browser.
    for (const source of [telecomClient, telephonyPanel]) {
      expect(source).not.toMatch(/\bfetch\(/);
      expect(source).not.toMatch(/XMLHttpRequest/);
    }
    for (const guide of CARRIER_GUIDES) {
      expect(guide.docsUrl).toMatch(/^https:\/\//);
    }
  });

  it("offers exactly the carriers the daemon can build", () => {
    // The mock carrier is real code and deliberately absent from this list: it
    // exists so CI never dials anyone, and an operator who could configure it
    // would have a number that silently answers nothing.
    const rustKinds = [...telephonyMod.matchAll(/"(twilio|telnyx|plivo|mock)" => Some\(/g)].map(
      (match) => match[1],
    );
    expect(new Set(rustKinds)).toEqual(new Set(["twilio", "telnyx", "plivo", "mock"]));
    expect(CARRIER_GUIDES.map((guide) => guide.kind)).toEqual(["twilio", "telnyx", "plivo"]);
  });

  it("asks for every non-secret setting the daemon reads", () => {
    // Telnyx cannot be built without its published webhook key — the daemon
    // refuses the account outright — so setup has to collect it.
    expect(telephonyMod).toContain('.get("webhook_public_key")');
    const telnyx = CARRIER_GUIDES.find((guide) => guide.kind === "telnyx");
    expect(telnyx?.configKeys).toContain("webhook_public_key");
  });

  it("never sends a carrier credential as a command argument", () => {
    // `telecom_set_credential` hands the secret to the CLI's own `set-token`
    // on stdin. A secret in an argument vector would show up in a process
    // listing — and the CLI has to be what writes the keychain entry, because
    // macOS gives an item back only to the executable that stored it and the
    // daemon that reads it is that same executable.
    const start = daemonCommands.indexOf("pub async fn telecom_set_credential(");
    expect(start).toBeGreaterThan(-1);
    const body = daemonCommands.slice(start, start + 1400);
    expect(body).toContain("command_with_stdin(");
    expect(body).toContain('"set-token".into()');
    expect(body).not.toContain("set_password");
    expect(body).not.toMatch(/args\.push\(secret/);
    expect(telecomCli).toContain("read_secret_from_stdin()");
  });
});
