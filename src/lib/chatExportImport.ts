import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";

export interface ImportedConversation { title: string; markdown: string }
export interface ChatImportResult { path: string; documentCount: number }

type UnknownRecord = Record<string, unknown>;
const record = (value: unknown): UnknownRecord | null => typeof value === "object" && value !== null && !Array.isArray(value) ? value as UnknownRecord : null;
const text = (value: unknown): string => typeof value === "string" ? value : "";
const array = (value: unknown): unknown[] => Array.isArray(value) ? value : [];

function messageText(message: UnknownRecord): string {
  const content = record(message.content);
  const parts = content ? array(content.parts) : [];
  if (parts.length) return parts.map((part) => typeof part === "string" ? part : text(record(part)?.text)).filter(Boolean).join("\n");
  const direct = message.text;
  if (typeof direct === "string") return direct;
  if (Array.isArray(direct)) return direct.map((part) => text(record(part)?.text) || text(part)).filter(Boolean).join("\n");
  return text(content?.text);
}

function chatGptConversation(raw: UnknownRecord, index: number): ImportedConversation | null {
  const mapping = record(raw.mapping);
  if (!mapping) return null;
  const rows: Array<{ role: string; body: string; time: number }> = [];
  for (const node of Object.values(mapping)) {
    const n = record(node); const message = record(n?.message); if (!message) continue;
    const author = record(message.author);
    const body = messageText(message).trim(); if (!body) continue;
    rows.push({ role: text(author?.role) || "unknown", body, time: Number(message.create_time ?? n?.create_time ?? 0) || 0 });
  }
  rows.sort((a, b) => a.time - b.time);
  if (!rows.length) return null;
  return { title: text(raw.title) || `Conversation ${index + 1}`, markdown: rows.map((row) => `## ${row.role}\n\n${row.body}`).join("\n\n") };
}

function claudeConversation(raw: UnknownRecord, index: number): ImportedConversation | null {
  const messages = array(raw.chat_messages ?? raw.messages);
  if (!messages.length) return null;
  const rows = messages.map(record).filter((m): m is UnknownRecord => !!m).map((m) => {
    const role = text(m.sender ?? m.role) || "unknown";
    let body = text(m.text);
    if (!body && Array.isArray(m.content)) body = array(m.content).map(record).filter((v): v is UnknownRecord => !!v).map((v) => text(v.text)).filter(Boolean).join("\n");
    return { role, body: body.trim() };
  }).filter((row) => row.body);
  if (!rows.length) return null;
  return { title: text(raw.name ?? raw.title) || `Conversation ${index + 1}`, markdown: rows.map((row) => `## ${row.role}\n\n${row.body}`).join("\n\n") };
}

export function parseChatExport(value: unknown): ImportedConversation[] {
  const root = record(value);
  const candidates = Array.isArray(value) ? value : array(root?.conversations ?? root?.chats ?? root?.items);
  const output: ImportedConversation[] = [];
  candidates.forEach((item, index) => {
    const row = record(item); if (!row) return;
    const parsed = row.mapping ? chatGptConversation(row, index) : claudeConversation(row, index);
    if (parsed) output.push(parsed);
  });
  if (!output.length && root) {
    const single = root.mapping ? chatGptConversation(root, 0) : claudeConversation(root, 0);
    if (single) output.push(single);
  }
  return output;
}

async function inflateRaw(bytes: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

async function jsonFromZip(bytes: Uint8Array): Promise<unknown> {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let eocd = -1;
  for (let offset = bytes.length - 22; offset >= Math.max(0, bytes.length - 65557); offset--) {
    if (view.getUint32(offset, true) === 0x06054b50) { eocd = offset; break; }
  }
  if (eocd < 0) throw new Error("ZIP end-of-directory record not found");
  const count = view.getUint16(eocd + 10, true);
  let cursor = view.getUint32(eocd + 16, true);
  const decoder = new TextDecoder();
  for (let i = 0; i < count; i++) {
    if (view.getUint32(cursor, true) !== 0x02014b50) throw new Error("Invalid ZIP central directory");
    const method = view.getUint16(cursor + 10, true);
    const compressedSize = view.getUint32(cursor + 20, true);
    const nameLength = view.getUint16(cursor + 28, true);
    const extraLength = view.getUint16(cursor + 30, true);
    const commentLength = view.getUint16(cursor + 32, true);
    const localOffset = view.getUint32(cursor + 42, true);
    const name = decoder.decode(bytes.slice(cursor + 46, cursor + 46 + nameLength));
    if (/((conversations|chats).*\.json|\.json)$/i.test(name) && !name.startsWith("__MACOSX/")) {
      if (view.getUint32(localOffset, true) !== 0x04034b50) throw new Error("Invalid ZIP local header");
      const localNameLength = view.getUint16(localOffset + 26, true);
      const localExtraLength = view.getUint16(localOffset + 28, true);
      const dataStart = localOffset + 30 + localNameLength + localExtraLength;
      const compressed = bytes.slice(dataStart, dataStart + compressedSize);
      const plain = method === 0 ? compressed : method === 8 ? await inflateRaw(compressed) : null;
      if (plain) {
        try {
          const parsed = JSON.parse(decoder.decode(plain));
          if (parseChatExport(parsed).length) return parsed;
        } catch { /* continue to another JSON member */ }
      }
    }
    cursor += 46 + nameLength + extraLength + commentLength;
  }
  throw new Error("No supported conversation JSON found in the ZIP");
}

export async function chooseAndParseChatExport(): Promise<{ sourceName: string; conversations: ImportedConversation[] } | null> {
  const picked = await open({ directory: false, multiple: false, filters: [{ name: "Conversation export", extensions: ["json", "zip"] }] });
  if (typeof picked !== "string") return null;
  const bytes = await readFile(picked);
  const parsed = picked.toLowerCase().endsWith(".zip") ? await jsonFromZip(bytes) : JSON.parse(new TextDecoder().decode(bytes));
  const conversations = parseChatExport(parsed);
  if (!conversations.length) throw new Error("No ChatGPT, Claude, or Gemini-style conversations were found");
  return { sourceName: picked.split(/[/\\]/).pop()?.replace(/\.(json|zip)$/i, "") || "chat-export", conversations };
}

export async function writeChatImport(sourceName: string, conversations: ImportedConversation[]): Promise<ChatImportResult> {
  return invoke<ChatImportResult>("chat_export_write_documents", { sourceName, documents: conversations });
}
