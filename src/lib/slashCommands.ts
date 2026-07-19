import type { ChatMessage } from "./llamaClient";

export type BuiltInSlashCommandName =
  | "status"
  | "tools"
  | "skills"
  | "plugins"
  | "model"
  | "new"
  | "compact"
  | "stop"
  | "usage"
  | "learn"
  | "btw";

export interface BuiltInSlashCommand {
  command: BuiltInSlashCommandName;
  name: string;
  description: string;
  usage: string;
}

export const BUILT_IN_SLASH_COMMANDS: readonly BuiltInSlashCommand[] = [
  { command: "status", name: "Status", description: "Show the active runtime, workspace, and connections.", usage: "/status" },
  { command: "tools", name: "Tools", description: "List tools currently available to a model turn.", usage: "/tools" },
  { command: "skills", name: "Skills", description: "List enabled skills and their invocation names.", usage: "/skills" },
  { command: "plugins", name: "Plugins", description: "List installed declarative plugins and health.", usage: "/plugins" },
  { command: "model", name: "Model", description: "Show or switch the active model.", usage: "/model [provider:model-or-name]" },
  { command: "new", name: "New chat", description: "Start a fresh chat without contacting a model.", usage: "/new" },
  { command: "compact", name: "Compact", description: "Compact older complete turns now.", usage: "/compact" },
  { command: "stop", name: "Stop", description: "Cancel the active turn.", usage: "/stop" },
  { command: "usage", name: "Usage", description: "Show real token usage reported for this chat.", usage: "/usage" },
  { command: "learn", name: "Learn skill", description: "Create a quarantined skill proposal for review.", usage: "/learn command | instructions" },
  { command: "btw", name: "Side question", description: "Open the Side Chat panel to ask a quick question without adding to the conversation.", usage: "/btw [question]" },
] as const;

const BUILT_IN_BY_NAME = new Map(BUILT_IN_SLASH_COMMANDS.map((entry) => [entry.command, entry]));

export interface ParsedBuiltInSlashCommand {
  definition: BuiltInSlashCommand;
  arguments: string;
}

/** Built-ins are deterministic and owner-local: only an exact first token is
 * consumed. Unknown slash text remains ordinary chat/skill input. */
export function parseBuiltInSlashCommand(text: string): ParsedBuiltInSlashCommand | null {
  const trimmed = text.trim();
  const match = /^\/([a-z0-9-]+)(?:\s+([\s\S]*))?$/.exec(trimmed);
  if (!match) return null;
  const definition = BUILT_IN_BY_NAME.get(match[1] as BuiltInSlashCommandName);
  if (!definition) return null;
  return { definition, arguments: (match[2] ?? "").trim() };
}

export const COMMAND_NOTICE_PREFIX = "[Command]";

export interface CommandNotice {
  command: BuiltInSlashCommandName;
  text: string;
  ok: boolean;
}

export function formatCommandNotice(notice: CommandNotice): string {
  return `${COMMAND_NOTICE_PREFIX}${JSON.stringify(notice)}`;
}

export function parseCommandNotice(message: ChatMessage): CommandNotice | null {
  if (message.role !== "system" || typeof message.content !== "string" || !message.content.startsWith(COMMAND_NOTICE_PREFIX)) {
    return null;
  }
  try {
    const candidate = JSON.parse(message.content.slice(COMMAND_NOTICE_PREFIX.length)) as Partial<CommandNotice>;
    if (
      typeof candidate.command !== "string" ||
      !BUILT_IN_BY_NAME.has(candidate.command as BuiltInSlashCommandName) ||
      typeof candidate.text !== "string" ||
      typeof candidate.ok !== "boolean"
    ) {
      return null;
    }
    return candidate as CommandNotice;
  } catch {
    return null;
  }
}

export function isCommandNotice(message: ChatMessage): boolean {
  return parseCommandNotice(message) !== null;
}

export const BTW_NOTICE_PREFIX = "[Btw]";

/** One `/btw` side-question exchange, persisted in the transcript for display
 * only. Every wire builder (agentLoop, compareRunner, crewRunner) strips these
 * with `isBtwNotice` before contacting a model — the whole point of `/btw` is
 * that the exchange never joins the conversation the model sees. */
export interface BtwNotice {
  question: string;
  answer: string;
  ok: boolean;
  /** False while the answer is still streaming in, so the renderer can show a
   * progress affordance that survives re-renders mid-stream. */
  done: boolean;
}

export function formatBtwNotice(notice: BtwNotice): string {
  return `${BTW_NOTICE_PREFIX}${JSON.stringify(notice)}`;
}

export function parseBtwNotice(message: ChatMessage): BtwNotice | null {
  if (message.role !== "system" || typeof message.content !== "string" || !message.content.startsWith(BTW_NOTICE_PREFIX)) {
    return null;
  }
  try {
    const candidate = JSON.parse(message.content.slice(BTW_NOTICE_PREFIX.length)) as Partial<BtwNotice>;
    if (
      typeof candidate.question !== "string" ||
      typeof candidate.answer !== "string" ||
      typeof candidate.ok !== "boolean" ||
      typeof candidate.done !== "boolean"
    ) {
      return null;
    }
    return candidate as BtwNotice;
  } catch {
    return null;
  }
}

export function isBtwNotice(message: ChatMessage): boolean {
  return parseBtwNotice(message) !== null;
}

export const SIDE_QUESTION_SYSTEM_PROMPT =
  "The user paused the conversation to ask a quick side question. Answer it directly and concisely, using the conversation so far as context where relevant. This exchange is shown to the user but will NOT be added to the conversation, so do not reference this answer in future turns and do not ask follow-up questions.";

/** The one-shot wire for a `/btw` call: a side-question system prompt, the
 * conversation so far (minus earlier side questions — they never accumulate),
 * and the question as the final user message. Pure so tests can pin it. */
export function buildSideQuestionWire(history: readonly ChatMessage[], question: string): ChatMessage[] {
  return [
    { role: "system", content: SIDE_QUESTION_SYSTEM_PROMPT },
    ...history.filter((message) => !isBtwNotice(message)),
    { role: "user", content: question },
  ];
}
