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
  | "learn";

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
