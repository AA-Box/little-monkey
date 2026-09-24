import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";

export type ChatExportPlatform = "chatgpt" | "claude" | "gemini" | "unknown";

export interface ImportedConversation {
  title: string;
  markdown: string;
  platform?: ChatExportPlatform;
  timestamp?: string | null;
  messageCount?: number;
}

export interface ChatImportResult {
  path: string;
  documentCount: number;
}

type UnknownRecord = Record<string, unknown>;

type ChatGptNode = {
  id: string;
  parent: string | null;
  children: string[];
  message: UnknownRecord | null;
};

const MAX_EXPORT_BYTES = 256 * 1024 * 1024;
const MAX_ZIP_ENTRY_BYTES = 128 * 1024 * 1024;

const record = (value: unknown): UnknownRecord | null =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as UnknownRecord)
    : null;
const text = (value: unknown): string => (typeof value === "string" ? value : "");
const array = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const idText = (value: unknown): string | null => {
  if (typeof value === "string" && value.trim()) return value;
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  return null;
};

function readableContent(value: unknown): string {
  if (typeof value === "string") return value.trim();
  const content = record(value);
  if (!content) return "";

  const chunks: string[] = [];
  for (const part of array(content.parts)) {
    if (typeof part === "string") {
      if (part.trim()) chunks.push(part.trim());
      continue;
    }
    const partRecord = record(part);
    const value = text(partRecord?.text).trim();
    if (value) chunks.push(value);
  }

  if (!chunks.length) {
    const flat = text(content.text).trim() || text(content.result).trim();
    if (flat) chunks.push(flat);
  }
  return chunks.join("\n").trim();
}

function markdownForRows(
  title: string,
  rows: Array<{ role: string; body: string }>,
  timestamp?: string | null,
): ImportedConversation | null {
  const readable = rows.filter((row) => row.body.trim());
  if (!readable.length) return null;

  const lines = [`# ${title}`, ""];
  if (timestamp) lines.push(`_Imported timestamp: ${timestamp}_`, "");
  for (const row of readable) {
    const role = row.role.toLowerCase();
    const heading =
      role === "user" || role === "human"
        ? "You"
        : role === "assistant" || role === "model"
          ? "Assistant"
          : row.role;
    lines.push(`## ${heading}`, "", row.body.trim(), "");
  }
  return {
    title,
    markdown: lines.join("\n").trim(),
    messageCount: readable.length,
  };
}

function chatGptNode(value: unknown, fallbackId: string): ChatGptNode | null {
  const node = record(value);
  if (!node) return null;
  const id = idText(node.id) ?? fallbackId;
  return {
    id,
    parent: idText(node.parent),
    children: array(node.children).map(idText).filter((item): item is string => Boolean(item)),
    message: record(node.message),
  };
}

function chatGptRows(nodes: ChatGptNode[]): Array<{ role: string; body: string }> {
  const rows: Array<{ role: string; body: string }> = [];
  for (const node of nodes) {
    const message = node.message;
    if (!message) continue;
    const role = text(record(message.author)?.role) || "unknown";
    if (role === "system" || role === "tool") continue;
    const body = readableContent(message.content);
    if (body) rows.push({ role, body });
  }
  return rows;
}

function parseChatGptConversation(raw: UnknownRecord, index: number): ImportedConversation | null {
  const mapping = record(raw.mapping);
  if (!mapping) return null;

  const nodes = new Map<string, ChatGptNode>();
  for (const [key, value] of Object.entries(mapping)) {
    const node = chatGptNode(value, key);
    if (!node) continue;
    nodes.set(key, node);
    nodes.set(node.id, node);
  }

  const walkParents = (leafId: string): ChatGptNode[] => {
    const chain: ChatGptNode[] = [];
    const seen = new Set<string>();
    let node = nodes.get(leafId);
    while (node && !seen.has(node.id)) {
      seen.add(node.id);
      chain.push(node);
      node = node.parent ? nodes.get(node.parent) : undefined;
    }
    return chain.reverse();
  };

  const walkNewestBranch = (root: ChatGptNode): ChatGptNode[] => {
    const chain: ChatGptNode[] = [];
    const seen = new Set<string>();
    let node: ChatGptNode | undefined = root;
    while (node && !seen.has(node.id)) {
      seen.add(node.id);
      chain.push(node);
      const nextId: string | undefined = node.children[node.children.length - 1];
      node = nextId ? nodes.get(nextId) : undefined;
    }
    return chain;
  };

  let best: ChatGptNode[] = [];
  const currentNode = idText(raw.current_node);
  if (currentNode && nodes.has(currentNode)) best = walkParents(currentNode);

  if (!chatGptRows(best).length) {
    const unique = [...new Map([...nodes.values()].map((node) => [node.id, node])).values()];
    const roots = unique.filter((node) => !node.parent || !nodes.has(node.parent));
    for (const root of roots.length ? roots : unique) {
      const candidate = walkNewestBranch(root);
      if (chatGptRows(candidate).length > chatGptRows(best).length) best = candidate;
    }
  }

  const title = text(raw.title).trim() || `Conversation ${index + 1}`;
  const timestampSeconds = Number(raw.update_time ?? raw.create_time ?? 0);
  const timestamp = Number.isFinite(timestampSeconds) && timestampSeconds > 0
    ? new Date(timestampSeconds * 1000).toISOString()
    : null;
  const parsed = markdownForRows(title, chatGptRows(best), timestamp);
  if (parsed) parsed.platform = "chatgpt";
  return parsed;
}

function claudeBody(message: UnknownRecord): string {
  const chunks: string[] = [];
  for (const block of array(message.content)) {
    const blockRecord = record(block);
    const value = text(blockRecord?.text).trim();
    if (value) chunks.push(value);
  }
  if (!chunks.length && text(message.text).trim()) chunks.push(text(message.text).trim());

  for (const attachment of array(message.attachments)) {
    const item = record(attachment);
    const extracted = text(item?.extracted_content).trim();
    if (!extracted) continue;
    const name = text(item?.file_name).trim();
    chunks.push(`${name ? `[attachment: ${name}]\n` : ""}${extracted}`);
  }
  return chunks.join("\n\n").trim();
}

function parseClaudeConversation(raw: UnknownRecord, index: number): ImportedConversation | null {
  const messages = array(raw.chat_messages ?? raw.messages);
  if (!messages.length) return null;
  const rows = messages
    .map(record)
    .filter((message): message is UnknownRecord => Boolean(message))
    .map((message) => ({
      role: text(message.sender ?? message.role) || "unknown",
      body: claudeBody(message),
    }));
  const title = text(raw.name ?? raw.title).trim() || `Conversation ${index + 1}`;
  const timestamp = text(raw.updated_at ?? raw.created_at).trim() || null;
  const parsed = markdownForRows(title, rows, timestamp);
  if (parsed) parsed.platform = "claude";
  return parsed;
}

function geminiBody(message: UnknownRecord): string {
  const flat = text(message.text).trim();
  if (flat) return flat;
  if (typeof message.content === "string") return message.content.trim();
  return array(message.content)
    .map((block) => (typeof block === "string" ? block.trim() : text(record(block)?.text).trim()))
    .filter(Boolean)
    .join("\n")
    .trim();
}

function parseGemini(value: unknown): ImportedConversation[] {
  const root = record(value);
  const items = Array.isArray(value) ? value : array(root?.activities);
  const output: ImportedConversation[] = [];

  items.forEach((item, index) => {
    const row = record(item);
    if (!row) return;
    const header = text(row.header).toLowerCase();
    const explicitGemini = header.includes("gemini") || header.includes("bard");
    const messages = array(row.messages);
    if (!explicitGemini && !messages.length) return;

    const title = text(row.title).trim().slice(0, 120) || `Gemini activity ${index + 1}`;
    const timestamp = text(row.time ?? row.timestamp).trim() || null;
    const rows = messages
      .map(record)
      .filter((message): message is UnknownRecord => Boolean(message))
      .map((message) => ({
        role: text(message.role ?? message.author) || "unknown",
        body: geminiBody(message),
      }));

    if (!rows.length && explicitGemini) {
      const body = text(row.text ?? row.description).trim() || title;
      rows.push({ role: "user", body });
    }
    const parsed = markdownForRows(title, rows, timestamp);
    if (parsed) {
      parsed.platform = "gemini";
      output.push(parsed);
    }
  });
  return output;
}

function candidateArray(value: unknown): unknown[] {
  if (Array.isArray(value)) return value;
  const root = record(value);
  return array(root?.conversations ?? root?.chats ?? root?.items);
}

function detectedPlatform(value: unknown): ChatExportPlatform {
  const root = record(value);
  if (root && Array.isArray(root.activities)) return "gemini";
  const sample = candidateArray(value).map(record).find(Boolean);
  if (!sample) return "unknown";
  if (record(sample.mapping)) return "chatgpt";
  if (Array.isArray(sample.chat_messages)) return "claude";
  const header = text(sample.header).toLowerCase();
  if (header.includes("gemini") || header.includes("bard")) return "gemini";
  return "unknown";
}

export function parseChatExport(value: unknown): ImportedConversation[] {
  const platform = detectedPlatform(value);
  if (platform === "gemini") return parseGemini(value);

  const candidates = candidateArray(value);
  const parseArray = (kind: "chatgpt" | "claude") => {
    const output: ImportedConversation[] = [];
    candidates.forEach((item, index) => {
      const row = record(item);
      if (!row) return;
      const parsed = kind === "chatgpt"
        ? parseChatGptConversation(row, index)
        : parseClaudeConversation(row, index);
      if (parsed) output.push(parsed);
    });
    return output;
  };

  if (platform === "chatgpt") return parseArray("chatgpt");
  if (platform === "claude") return parseArray("claude");

  const attempts = [parseArray("chatgpt"), parseArray("claude"), parseGemini(value)];
  attempts.sort((left, right) => right.length - left.length);
  return attempts[0];
}

async function inflateRaw(bytes: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

type ZipEntry = {
  name: string;
  method: number;
  compressedSize: number;
  uncompressedSize: number;
  localOffset: number;
};

function zipEntries(bytes: Uint8Array): ZipEntry[] {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let eocd = -1;
  for (let offset = bytes.length - 22; offset >= Math.max(0, bytes.length - 65557); offset--) {
    if (view.getUint32(offset, true) === 0x06054b50) {
      eocd = offset;
      break;
    }
  }
  if (eocd < 0) throw new Error("ZIP end-of-directory record not found");

  const count = view.getUint16(eocd + 10, true);
  let cursor = view.getUint32(eocd + 16, true);
  const decoder = new TextDecoder();
  const entries: ZipEntry[] = [];
  for (let index = 0; index < count; index++) {
    if (cursor + 46 > bytes.length || view.getUint32(cursor, true) !== 0x02014b50) {
      throw new Error("Invalid ZIP central directory");
    }
    const nameLength = view.getUint16(cursor + 28, true);
    const extraLength = view.getUint16(cursor + 30, true);
    const commentLength = view.getUint16(cursor + 32, true);
    entries.push({
      method: view.getUint16(cursor + 10, true),
      compressedSize: view.getUint32(cursor + 20, true),
      uncompressedSize: view.getUint32(cursor + 24, true),
      localOffset: view.getUint32(cursor + 42, true),
      name: decoder.decode(bytes.slice(cursor + 46, cursor + 46 + nameLength)),
    });
    cursor += 46 + nameLength + extraLength + commentLength;
  }
  return entries;
}

async function readZipEntry(bytes: Uint8Array, entry: ZipEntry): Promise<Uint8Array | null> {
  if (entry.uncompressedSize > MAX_ZIP_ENTRY_BYTES) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (entry.localOffset + 30 > bytes.length || view.getUint32(entry.localOffset, true) !== 0x04034b50) {
    return null;
  }
  const nameLength = view.getUint16(entry.localOffset + 26, true);
  const extraLength = view.getUint16(entry.localOffset + 28, true);
  const start = entry.localOffset + 30 + nameLength + extraLength;
  if (start + entry.compressedSize > bytes.length) return null;
  const compressed = bytes.slice(start, start + entry.compressedSize);
  if (entry.method === 0) return compressed;
  if (entry.method === 8) return inflateRaw(compressed);
  return null;
}

async function conversationsFromZip(bytes: Uint8Array): Promise<ImportedConversation[]> {
  const decoder = new TextDecoder();
  const entries = zipEntries(bytes).filter((entry) => {
    const name = entry.name.toLowerCase();
    return name.endsWith(".json") && !name.startsWith("__macosx/");
  });
  const basename = (name: string) => name.split(/[\\/]/).pop()?.toLowerCase() ?? "";
  const conversationFiles = entries
    .filter((entry) => /^(conversations|conversations-\d+)\.json$/.test(basename(entry.name)))
    .sort((left, right) => basename(left.name).localeCompare(basename(right.name), "en", { numeric: true }));

  const parseEntry = async (entry: ZipEntry) => {
    const plain = await readZipEntry(bytes, entry);
    if (!plain || plain.length > MAX_ZIP_ENTRY_BYTES) return [];
    try {
      return parseChatExport(JSON.parse(decoder.decode(plain)));
    } catch {
      return [];
    }
  };

  if (conversationFiles.length) {
    const merged: ImportedConversation[] = [];
    for (const entry of conversationFiles) merged.push(...await parseEntry(entry));
    if (merged.length) return merged;
  }

  let best: ImportedConversation[] = [];
  for (const entry of entries) {
    const parsed = await parseEntry(entry);
    if (parsed.length > best.length) best = parsed;
  }
  return best;
}

export async function chooseAndParseChatExport(): Promise<{
  sourceName: string;
  conversations: ImportedConversation[];
} | null> {
  const picked = await open({
    directory: false,
    multiple: false,
    filters: [{ name: "Conversation export", extensions: ["json", "zip"] }],
  });
  if (typeof picked !== "string") return null;

  const bytes = await readFile(picked);
  if (bytes.length > MAX_EXPORT_BYTES) {
    throw new Error("Conversation export exceeds the 256 MiB import limit");
  }
  const conversations = picked.toLowerCase().endsWith(".zip")
    ? await conversationsFromZip(bytes)
    : parseChatExport(JSON.parse(new TextDecoder().decode(bytes)));
  if (!conversations.length) {
    throw new Error("No supported ChatGPT, Claude, or Gemini conversations were found");
  }
  return {
    sourceName: picked.split(/[/\\]/).pop()?.replace(/\.(json|zip)$/i, "") || "chat-export",
    conversations,
  };
}

export async function writeChatImport(
  sourceName: string,
  conversations: ImportedConversation[],
): Promise<ChatImportResult> {
  return invoke<ChatImportResult>("chat_export_write_documents", { sourceName, documents: conversations });
}
