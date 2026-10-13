/** Safe, resumable migration of user-selected coding-agent transcripts.
 * No imported tool calls, reasoning blocks, tool results, settings or grants
 * are ever executed or supplied to the model as instructions.
 */
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";
import type { ChatMessage } from "./llamaClient";
import {
  useSessionStore, flushSessionPersistence,
  planPortableSessionImport, portableSessionPlanPayload, applyPortableSessionImportPlan,
  type ChatSession,
} from "../store/sessionStore";

type Obj = Record<string, unknown>;
export type CodingFormat = "claude-code" | "codex" | "pi" | "opencode";
export interface CodingTranscript {
  format: CodingFormat;
  title: string;
  messages: ChatMessage[];
  createdAt: number;
}
const MAX_BYTES = 32 * 1024 * 1024;
const obj = (x: unknown): Obj | null => typeof x === "object" && x !== null && !Array.isArray(x) ? x as Obj : null;
const str = (x: unknown): string => typeof x === "string" ? x : "";
const arr = (x: unknown): unknown[] => Array.isArray(x) ? x : [];
const time = (x: unknown): number => {
  if (typeof x === "number" && Number.isFinite(x)) return x < 1e11 ? x * 1000 : x;
  const n = Date.parse(str(x));
  return Number.isFinite(n) ? n : Date.now();
};
function visibleText(value: unknown): string {
  if (typeof value === "string") return value.slice(0, 1_000_000);
  const data = obj(value);
  if (data) return visibleText(data.content ?? data.text);
  return arr(value).flatMap(part => {
    if (typeof part === "string") return [part];
    const row = obj(part);
    // "thinking", "tool_use", "tool_result", "reasoning" are excluded.
    return row && ["text","input_text","output_text"].includes(str(row.type))
      ? [str(row.text)] : [];
  }).join("\n").slice(0, 1_000_000);
}
function append(out: ChatMessage[], role: unknown, content: unknown, date?: unknown): void {
  const r = str(role);
  if (!["user", "human", "assistant"].includes(r)) return;
  const body = visibleText(content).trim();
  if (body) out.push({ role: r === "human" ? "user" : r as "user" | "assistant", content: body,
    ...(date == null ? {} : { at: time(date) }) });
}
function finish(format: CodingFormat, messages: ChatMessage[], createdAt: number, suggested?: string): CodingTranscript | null {
  if (!messages.length) return null;
  const title = suggested?.trim() || str(messages.find(m => m.role === "user")?.content).replace(/\s+/g," ").slice(0,90) || "Coding session";
  return { format, messages, createdAt, title: title.slice(0,160) };
}
function jsonl(text: string): Obj[] {
  const lines = text.split(/\r?\n/);
  if (lines.length > 150_000) throw new Error("Coding transcript contains too many events");
  const out: Obj[] = [];
  for (const line of lines) {
    if (!line.trim()) continue;
    let parsed: unknown;
    try { parsed = JSON.parse(line); } catch { throw new Error("Invalid coding transcript JSONL"); }
    const entry = obj(parsed);
    if (entry) out.push(entry);
  }
  return out;
}
function fromClaude(items: Obj[]): CodingTranscript | null {
  const messages: ChatMessage[] = [];
  const positions = new Map<string, number>();
  let start = Number.POSITIVE_INFINITY;
  for (const row of items) {
    if (!["user","assistant"].includes(str(row.type)) || row.isSidechain === true || row.isMeta === true) continue;
    const msg = obj(row.message);
    if (!msg) continue;
    const text: ChatMessage[] = [];
    append(text,msg.role,msg.content,row.timestamp);
    if (!text.length) continue;
    const id = str(msg.id) || str(row.uuid);
    if (id && positions.has(id)) {
      const index = positions.get(id)!;
      const prior = str(messages[index].content);
      const next = str(text[0].content);
      // Claude Code emits each visible content block as a separate JSONL
      // record under the same message.id. Some releases also emit cumulative
      // snapshots. Keep disjoint blocks, but collapse duplicate/cumulative
      // snapshots instead of either discarding text or duplicating it.
      if (next.startsWith(prior)) messages[index] = text[0];
      else if (prior !== next && !prior.startsWith(next))
        messages[index] = { ...messages[index], content: prior + "\\n" + next };
    } else {
      if (id) positions.set(id,messages.length);
      messages.push(text[0]);
    }
    if (row.timestamp) start = Math.min(start,time(row.timestamp));
  }
  return finish("claude-code",messages,Number.isFinite(start)?start:Date.now());
}
function fromCodex(items: Obj[]): CodingTranscript | null {
  const primary: ChatMessage[] = [], fallback: ChatMessage[] = [];
  let start = Date.now();
  for (const row of items) {
    const p = obj(row.payload);
    if (!p) continue;
    if (row.type === "session_meta") start = time(p.timestamp ?? row.timestamp);
    if (row.type === "response_item" && p.type === "message") append(primary,p.role,p.content,row.timestamp);
    else if (row.type === "event_msg" && p.type === "user_message") append(fallback,"user",p.message,row.timestamp);
    else if (row.type === "event_msg" && p.type === "agent_message") append(fallback,"assistant",p.message,row.timestamp);
  }
  // response_item and event_msg often duplicate the same conversation.
  // Some Codex versions emit user prompts only as event_msg, while assistant
  // output still uses response_item. Preserve both roles without duplicating
  // transcript events that occur on both channels.
  const hasUser = primary.some(m => m.role === "user");
  const hasAssistant = primary.some(m => m.role === "assistant");
  const messages = [
    ...(!hasUser ? fallback.filter(m => m.role === "user") : []),
    ...primary,
    ...(!hasAssistant ? fallback.filter(m => m.role === "assistant") : []),
  ].sort((a,b) => (a.at ?? 0) - (b.at ?? 0));
  return finish("codex",messages.length ? messages : fallback,start);
}
function fromPi(items: Obj[]): CodingTranscript | null {
  const header = items.find(x=>x.type==="session");
  const nodes = new Map<string,Obj>();
  let tip: string | null = null;
  for (const item of items) if (item.type !== "session" && str(item.id)) {
    nodes.set(str(item.id),item);
    tip = str(item.id);
  }
  const ancestry: Obj[] = [];
  const seen = new Set<string>();
  while (tip && nodes.has(tip) && !seen.has(tip)) {
    seen.add(tip);
    const node = nodes.get(tip)!;
    ancestry.push(node);
    tip = str(node.parentId) || null;
  }
  const messages: ChatMessage[] = [];
  for (const entry of ancestry.reverse()) {
    if (entry.type !== "message") continue;
    const msg = obj(entry.message);
    if (msg) append(messages,msg.role,msg.content,entry.timestamp);
  }
  const suggested = items.filter(x=>x.type==="session_info").map(x=>str(x.name)).filter(Boolean).pop();
  return finish("pi",messages,time(header?.timestamp),suggested);
}
function fromOpenCode(payload: Obj): CodingTranscript | null {
  const doc = obj(payload.data) ?? payload;
  const info = obj(doc.info) ?? doc;
  const messages: ChatMessage[] = [];
  for (const m of arr(doc.messages)) {
    const row = obj(m);
    if (!row) continue;
    const meta = obj(row.info) ?? row;
    append(messages,meta.role,row.parts ?? row.content ?? row.text,obj(meta.time)?.created);
  }
  return finish("opencode",messages,time(obj(info.time)?.created ?? info.createdAt),str(info.title));
}
export function parseCodingSession(source: string): CodingTranscript | null {
  if (new TextEncoder().encode(source).length > MAX_BYTES) throw new Error("Session exceeds 32 MiB limit");
  if (!source.trim()) return null;
  // OpenCode "export" is one JSON object; other tools persist JSONL events.
  try {
    const doc = obj(JSON.parse(source));
    if (doc) { const imported = fromOpenCode(doc); if (imported) return imported; }
  } catch { /* try JSONL */ }
  const rows = jsonl(source);
  if (rows.some(r=>r.type==="session_meta")) return fromCodex(rows);
  if (rows.some(r=>r.type==="session" && typeof r.cwd==="string")) return fromPi(rows);
  if (rows.some(r=>(r.type==="assistant" || r.type==="user") && obj(r.message))) return fromClaude(rows);
  return null;
}
function toNative(session: CodingTranscript, id: string): ChatSession {
  const now = Date.now();
  return {
    id, title: session.title, messages: session.messages.map(m=>structuredClone(m)),
    createdAt: session.createdAt, updatedAt: now,
    pinned: false, unread: true, archived: false, groupId: null,
    modelTarget: null, comparisonBranch: null, crewRun: null,
    // A foreign cwd never becomes an approved execution workspace.
    workspacePath: null, personaId: null, attachedStackIds: [], docChatMode: false,
    subagentRuns: {}, subagentRunMeta: {}, workflowRunMeta: {},
    messageTranslations: [], threadTranslations: [], displayTranslationLocale: null,
  };
}
/** Import user-selected files and synchronously commit the whole session
 * snapshot before updating the UI. Identical files are idempotent. */
export async function importCodingSessionFiles(): Promise<number> {
  const picked = await open({multiple:true,directory:false,
    filters:[{name:"Coding session",extensions:["jsonl","json"]}]});
  if (!picked) return 0;
  const files = typeof picked === "string" ? [picked] : picked;
  if (files.length > 20) throw new Error("Select at most 20 coding sessions per import");
  const candidates: ChatSession[] = [];
  for (const path of files) {
    const bytes = await readFile(path);
    if (bytes.length > MAX_BYTES) throw new Error("Coding session exceeds 32 MiB limit");
    const transcript = parseCodingSession(new TextDecoder("utf-8",{fatal:true}).decode(bytes));
    if (!transcript) throw new Error("Unsupported or empty coding session: " + path.split(/[/\\]/).pop());
    const digest = new Uint8Array(await crypto.subtle.digest("SHA-256",new Uint8Array(bytes).buffer));
    const hash = Array.from(digest,b=>b.toString(16).padStart(2,"0")).join("");
    candidates.push(toNative(transcript,"coding-import-"+transcript.format+"-"+hash));
  }
  // A queued debounce must not overwrite the newly imported snapshot.
  await flushSessionPersistence();
  const state = useSessionStore.getState();
  if (Object.keys(state.runningTurns).length || Object.keys(state.runningCrews).length)
    throw new Error("Stop active agent turns before importing sessions");
  const existing = new Set(state.sessions.map(x=>x.id));
  const incoming = candidates.filter(x => {
    if (existing.has(x.id)) return false;
    existing.add(x.id);
    return true;
  });
  if (!incoming.length) return 0;
  const plan = planPortableSessionImport(state,incoming,"merge");
  await invoke("sessions_save",{payload:portableSessionPlanPayload(plan)});
  applyPortableSessionImportPlan(plan);
  return plan.imported;
}
