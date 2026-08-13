/**
 * The Channels bridge contract: what the panel sends is what the daemon's
 * commands take.
 *
 * These calls are fixed-argument Tauri commands, so a mismatch is not a type
 * error anywhere — `invoke` takes `unknown`, and a renamed field simply
 * arrives as `undefined`. That failure is silent and looks like a routing bug
 * days later: a thread-scoped route quietly stored as an account-scoped one.
 * So the field names are compared against the Rust source that receives them,
 * the same way `contractDrift.test.ts` compares the published tool contract.
 *
 * Only names and registration are checked here. Whether a scope is legal, and
 * whether two routes tie, stays the daemon's answer — the frontend must not
 * hold a second copy of the routing rules.
 */
import { readFileSync } from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import {
  PROVIDER_GUIDES,
  UNIVERSAL_CONFIG_FIELDS,
  type ProviderConfigField,
  type RouteOptions,
} from "./channelsClient";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../");

const daemonCommands = readFileSync(
  path.join(REPO_ROOT, "src-tauri/src/daemon_commands.rs"),
  "utf8",
);
const libRs = readFileSync(path.join(REPO_ROOT, "src-tauri/src/lib.rs"), "utf8");
const channelsClient = readFileSync(path.join(REPO_ROOT, "src/lib/channelsClient.ts"), "utf8");

/** Every field of one `pub struct` in the Rust source, in declaration order. */
function rustStructFields(source: string, name: string): string[] {
  const start = source.indexOf(`pub struct ${name} {`);
  expect(start, `${name} not found in daemon_commands.rs`).toBeGreaterThan(-1);
  const body = source.slice(start, source.indexOf("\n}", start));
  return [...body.matchAll(/^\s{4}pub (\w+):/gm)].map((match) => match[1]);
}

describe("the channels bridge", () => {
  it("registers every command the panel invokes", () => {
    const invoked = [
      ...new Set([...channelsClient.matchAll(/invoke<[^>]*>\(\s*"(channels_\w+)"/g)].map((m) => m[1])),
    ];
    // A guard against the regex silently matching nothing and the rest of
    // this test passing vacuously.
    expect(invoked.length).toBeGreaterThan(10);
    for (const command of invoked) {
      expect(daemonCommands, `${command} has no #[tauri::command]`).toContain(
        `pub async fn ${command}(`,
      );
      expect(libRs, `${command} is not in generate_handler!`).toContain(
        `daemon_commands::${command},`,
      );
    }
  });

  it("sends exactly the route fields the daemon accepts", () => {
    // Typed as Required<RouteOptions>, so a field added to the TypeScript
    // side without a Rust counterpart fails to compile here, and one added in
    // Rust without a TypeScript counterpart fails the comparison below.
    const everyField: Required<RouteOptions> = {
      account_id: null,
      conversation_id: null,
      thread_id: null,
      sender_id: null,
      kind: null,
      repository: null,
      params: [],
      session_scope: null,
      priority: null,
      reply: null,
      enabled: null,
    };
    expect(rustStructFields(daemonCommands, "RouteOptionArgs").sort()).toEqual(
      Object.keys(everyField).sort(),
    );
  });

  it("covers the whole routing ladder from the bridge", () => {
    // The rungs the daemon resolves in order. Missing one here means a level
    // that can be reached by a message but not configured by an operator,
    // which is the gap this work exists to close.
    for (const field of ["account_id", "conversation_id", "thread_id", "sender_id", "kind"]) {
      expect(rustStructFields(daemonCommands, "RouteOptionArgs")).toContain(field);
    }
  });

  it("never composes a callback host itself", () => {
    // The daemon is the only authority on what it is reachable as. A frontend
    // that glues `window.location.origin` onto a path hands the operator a
    // URL nothing answers.
    for (const file of [
      "src/lib/channelsClient.ts",
      "src/components/Settings/ChannelsPanel.tsx",
      "src/components/Settings/ChannelRoutesSection.tsx",
    ]) {
      expect(readFileSync(path.join(REPO_ROOT, file), "utf8")).not.toMatch(
        /window\.location/,
      );
    }
  });

  it("never routes a provider request through the frontend", () => {
    // Every channel call is a daemon command. A request issued here would be
    // the app talking to Telegram or Slack directly, outside the daemon's
    // egress policy, its credential handling and its durable event log. The
    // provider URLs this file does hold are documentation links, opened in a
    // browser rather than called.
    for (const source of [
      channelsClient,
      readFileSync(path.join(REPO_ROOT, "src/components/Settings/ChannelsPanel.tsx"), "utf8"),
      readFileSync(path.join(REPO_ROOT, "src/components/Settings/ChannelRoutesSection.tsx"), "utf8"),
    ]) {
      expect(source).not.toMatch(/\bfetch\(/);
      expect(source).not.toMatch(/XMLHttpRequest/);
    }
  });
});

describe("the provider configuration contract", () => {
  // The daemon's schema of what each adapter actually reads, parsed from the
  // same source `channels set-config` validates against. Sharing one
  // definition across Rust and TypeScript would be disproportionate; holding
  // the two together with this test is the agreed alternative — a key added
  // on either side without the other fails here by name.
  const adaptersMod = readFileSync(
    path.join(REPO_ROOT, "src-tauri/src/bin/monkey-cli/daemon/adapters/mod.rs"),
    "utf8",
  );
  const channelTypes = readFileSync(
    path.join(REPO_ROOT, "src-tauri/src/channels/types.rs"),
    "utf8",
  );

  interface BackendField {
    key: string;
    required: boolean;
    kind: string;
  }

  function fieldEntries(body: string): BackendField[] {
    return [...body.matchAll(/(required|optional)\("(\w+)", ConfigFieldKind::(\w+)\)/g)].map(
      (match) => ({ key: match[2], required: match[1] === "required", kind: match[3] }),
    );
  }

  /** ChannelKind variant name -> wire string, from `as_str`. The label arms
   * never match because every label contains an uppercase letter. */
  const wireNames = new Map(
    [...channelTypes.matchAll(/ChannelKind::(\w+) => "([a-z_]+)"/g)].map((match) => [
      match[1],
      match[2],
    ]),
  );

  /** wire kind -> the daemon's editable fields for it. */
  function backendSchema(): Map<string, BackendField[]> {
    const start = adaptersMod.indexOf("pub(crate) fn config_fields");
    expect(start, "config_fields not found in adapters/mod.rs").toBeGreaterThan(-1);
    const body = adaptersMod.slice(start, adaptersMod.indexOf("\npub(crate) fn", start + 1));
    const consts = new Map(
      [...body.matchAll(/const (\w+): &\[ConfigField\] = &\[([\s\S]*?)\];/g)].map((match) => [
        match[1],
        fieldEntries(match[2]),
      ]),
    );
    const matchBody = body.slice(body.indexOf("match kind {"));
    const schema = new Map<string, BackendField[]>();
    for (const arm of matchBody.matchAll(/((?:ChannelKind::\w+\s*\|?\s*)+)=>\s*\{?\s*([A-Z_]+)/g)) {
      const fields = consts.get(arm[2]);
      // `Sms => return Err(...)` in build_adapter is outside this slice; an
      // arm naming an unknown const would be a parse failure worth failing on.
      expect(fields, `config_fields arm uses unknown const ${arm[2]}`).toBeDefined();
      for (const variant of arm[1].matchAll(/ChannelKind::(\w+)/g)) {
        const wire = wireNames.get(variant[1]);
        expect(wire, `no wire name for ChannelKind::${variant[1]}`).toBeDefined();
        schema.set(wire as string, fields as BackendField[]);
      }
    }
    // A parse that silently matched nothing must not pass vacuously.
    expect(schema.size).toBeGreaterThanOrEqual(13);
    return schema;
  }

  const TYPE_OF_KIND: Record<string, ProviderConfigField["type"]> = {
    Text: "text",
    Number: "number",
    Boolean: "boolean",
    TextList: "list",
  };

  it("gives every daemon-editable provider key a typed field in the frontend, and nothing more", () => {
    const schema = backendSchema();
    for (const [wire, backendFields] of schema) {
      const guide = PROVIDER_GUIDES.find((entry) => entry.kind === wire);
      expect(guide, `no provider guide for '${wire}'`).toBeDefined();
      const frontendFields = guide?.configFields ?? [];
      expect(
        frontendFields.map((field) => field.key).sort(),
        `frontend fields for '${wire}' drifted from config_fields()`,
      ).toEqual(backendFields.map((field) => field.key).sort());
      for (const backendField of backendFields) {
        const frontendField = frontendFields.find((field) => field.key === backendField.key);
        expect(
          frontendField?.type,
          `'${wire}.${backendField.key}' is typed differently on the two sides`,
        ).toBe(TYPE_OF_KIND[backendField.kind]);
        expect(
          frontendField?.required ?? false,
          `'${wire}.${backendField.key}' required flag drifted`,
        ).toBe(backendField.required);
      }
    }
    // And no guide invents a provider the daemon does not have.
    for (const guide of PROVIDER_GUIDES) {
      expect(schema.has(guide.kind), `guide '${guide.kind}' has no daemon schema`).toBe(true);
    }
  });

  it("edits the same universal attachment knobs the daemon accepts everywhere", () => {
    const start = adaptersMod.indexOf("const UNIVERSAL_CONFIG_FIELDS");
    expect(start).toBeGreaterThan(-1);
    const body = adaptersMod.slice(start, adaptersMod.indexOf("];", start));
    expect(fieldEntries(body).map((field) => field.key).sort()).toEqual(
      UNIVERSAL_CONFIG_FIELDS.map((field) => field.key).sort(),
    );
    // Ceilings exist on the daemon side; the frontend only collects numbers.
    for (const field of UNIVERSAL_CONFIG_FIELDS) {
      expect(field.type).toBe("number");
      expect(field.required ?? false).toBe(false);
    }
  });

  it("keeps every secret out of the non-secret schema on both sides", () => {
    const schema = backendSchema();
    for (const guide of PROVIDER_GUIDES) {
      for (const secret of guide.secretFields ?? []) {
        expect(
          schema.get(guide.kind)?.some((field) => field.key === secret.key) ?? false,
          `'${guide.kind}.${secret.key}' is a secret and must not be an account setting`,
        ).toBe(false);
        expect(guide.configFields.some((field) => field.key === secret.key)).toBe(false);
      }
    }
  });
});
