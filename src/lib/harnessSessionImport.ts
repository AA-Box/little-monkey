/**
 * Lossless-for-text, non-executable import of coding-harness session logs.
 *
 * Historical tool results are rendered as inert, clearly labelled excerpts;
 * never synthesize tool_call IDs, grants, checkpoints, active runs, or external
 * workspace permissions. A resumed Little Monkey session starts a NEW turn.
 */
import type { ChatMessage } from './llamaClient';
import { wrapUntrustedContent } from './untrustedContent';

export type Harness = 'codex' | 'claude-code' | 'pi' | 'opencode';
export interface HarnessImport {
  harness: Harness;
  sessionId: string | null;
  title: string;
  messages: ChatMessage[];
  skipped: number;
  truncated: boolean;
}

const MAX_SOURCE_BYTES = 16 * 1024 * 1024;
const MAX_MESSAGES = 400;
const MAX_MESSAGE_CHARS = 12_000;
const MAX_TOOL_CHARS = 2000;
const MAX_SESSION_CHARS = 250_000;
type Row = Record<string, unknown>;
const object = (x: unknown): Row | null => x && typeof x === 'object' && !Array.isArray(x) ? x as Row : null;
const str = (x: unknown): string => typeof x === 'string' ? x : '';
const arr = (x: unknown): unknown[] => Array.isArray(x) ? x : [];

function contentString(value: unknown, toolOutput = false): string {
  const cap = toolOutput ? MAX_TOOL_CHARS : MAX_MESSAGE_CHARS;
  if (typeof value === 'string') return value.slice(0, cap);
  const parts = Array.isArray(value) ? value : [value];
  const out: string[] = [];
  for (const raw of parts.slice(0, 50)) {
    if (typeof raw === 'string') { out.push(raw); continue; }
    const item = object(raw);
    if (!item) continue;
    const kind = str(item.type);
    if (['text','input_text','output_text','text_delta','reasoning'].includes(kind)) {
      if (kind === 'reasoning') continue; // Private reasoning must not be imported as chat context
      out.push(str(item.text) || str(item.content) || str(item.delta));
    } else if (kind === 'tool_use' || kind === 'function_call') {
      out.push('[Imported tool call: ' + (str(item.name) || 'tool').slice(0,120) +
        ' — historical only, not executable]');
    } else if (kind === 'tool_result' || kind === 'function_call_output') {
      const snippet = contentString(item.content ?? item.output, true);
      if (snippet) out.push('[Historical untrusted tool output]\n' +
        wrapUntrustedContent('imported coding agent tool output', snippet));
    }
  }
  return out.filter(Boolean).join('\n').slice(0, cap);
}

function classify(row: Row): {role: 'user'|'assistant'; text: string} | null {
  const type = str(row.type);
  const payload = object(row.payload);
  const wrapped = object(row.message);

  // Codex CLI rollouts: response_item / event_msg / user_message.
  if (type === 'response_item' && payload) {
    const kind = str(payload.type);
    if (kind === 'message') {
      const role = str(payload.role);
      if (role !== 'user' && role !== 'assistant') return null;
      return {role, text:contentString(payload.content)};
    }
    if (kind === 'function_call' || kind === 'custom_tool_call') {
      return {role:'assistant',text:'[Imported tool call: ' +
        str(payload.name).slice(0,120) + ' — historical only, not executable]'};
    }
    if (kind === 'function_call_output' || kind === 'custom_tool_call_output') {
      return {role:'assistant',text:'[Historical tool result]\n' +
        wrapUntrustedContent('imported Codex tool output',
          contentString(payload.output, true))};
    }
    return null;
  }
  if (type === 'event_msg' && payload) {
    const kind = str(payload.type);
    if (kind === 'user_message') return {role:'user',text:contentString(payload.message)};
    if (kind === 'agent_message') return {role:'assistant',text:contentString(payload.message)};
    return null;
  }

  // Claude Code, Pi, and OpenCode JSON export, with tool_use handled as
  // inert text instead of inventing OpenAI tool messages.
  const source = wrapped ?? object(row.info) ?? row;
  const role = str(source.role) || (type === 'user' ? 'user' : type === 'assistant' ? 'assistant' : '');
  if (role !== 'user' && role !== 'assistant') return null;
  const raw = source.content ?? source.text ?? row.parts ?? row.content ?? row.text;
  const text = contentString(raw);
  return text ? {role,text} : null;
}

function identify(raw: Row[]): Harness {
  if (raw.some(r=>['response_item','event_msg','session_meta'].includes(str(r.type)))) return 'codex';
  if (raw.some(r=>Boolean(r.sessionId) && ['assistant','user'].includes(str(r.type)))) return 'claude-code';
  if (raw.some(r=>str(r.type)==='session' || (str(r.type)==='message' && object(r.message)?.role))) return 'pi';
  if (raw.some(r=>Boolean(r.info) || Boolean(r.parts))) return 'opencode';
  throw new Error('Unsupported coding-agent export. Expected Claude Code, Codex, Pi, or OpenCode JSON/JSONL.');
}

function rawRows(text: string): Row[] {
  if (new TextEncoder().encode(text).byteLength > MAX_SOURCE_BYTES)
    throw new Error('Coding session exceeds the 16 MiB import limit.');
  const trimmed = text.trim();
  if (!trimmed) throw new Error('Coding-session file is empty.');
  if (trimmed[0] === '[' || (trimmed[0] === '{' && !trimmed.includes('\n'))) {
    const raw: unknown = JSON.parse(trimmed);
    const source = Array.isArray(raw) ? raw : object(raw)?.messages;
    const rows = Array.isArray(source) ? source : [raw];
    return rows.map(object).filter((x):x is Row => !!x);
  }
  const rows: Row[] = [];
  for (const line of trimmed.split(/\r?\n/)) {
    if (!line.trim()) continue;
    try {
      const record = object(JSON.parse(line));
      if (record) rows.push(record);
    } catch { throw new Error('Invalid JSONL line in external coding-agent session.'); }
    if (rows.length > 12_000) throw new Error('Too many source events in coding-agent session.');
  }
  return rows;
}

export function parseHarnessSession(source: string): HarnessImport {
  const rows = rawRows(source);
  const harness = identify(rows);
  const sessions = new Set(rows.map(row=>
    str(row.sessionId) || str(object(row.payload)?.session_id) ||
    str(object(object(row.payload)?.session)?.id) || str(row.session_id))
    .filter(Boolean));
  if (sessions.size > 1) throw new Error('Export contains multiple sessions; select a single session file.');
  const id = [...sessions][0] ?? null;
  const messages: ChatMessage[] = [];
  const seen = new Set<string>();
  let skipped = 0;
  let consumed = 0;
  let truncated = false;
  for (const row of rows) {
    const parsed = classify(row);
    if (!parsed || !parsed.text.trim()) {skipped++; continue;}
    // Claude Code often writes repeated assistant snapshots with the same
    // uuid. Deduplicate exact repeated rows, not genuinely distinct turns.
    const uuid = str(row.uuid);
    if (uuid) {
      if (seen.has(uuid)) {skipped++;continue;}
      seen.add(uuid);
    }
    const text = parsed.text.slice(0,MAX_MESSAGE_CHARS);
    if (messages.length >= MAX_MESSAGES || consumed + text.length > MAX_SESSION_CHARS) {
      truncated = true;break;
    }
    messages.push({role:parsed.role,content:text});
    consumed += text.length;
  }
  if (!messages.length) throw new Error('No safe user/assistant messages were found in coding-agent session.');
  const titleText = messages.find(m=>m.role==='user')?.content;
  const title = (typeof titleText === 'string' ? titleText : '')
    .replace(/\s+/g,' ').trim().slice(0,52) || 'Imported coding session';
  return {harness,sessionId:id,title,messages,skipped,truncated};
}
