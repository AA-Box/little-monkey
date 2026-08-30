import type { VerifyCommand, VerifyConfig } from "../store/verifyStore";

export interface VerificationCommandPlan {
  commands: VerifyCommand[];
  missingRequiredIds: string[];
}

/**
 * Builds the exact post-mutation verification command set for one operation.
 * Global Verification contributes every enabled command only when its toggle
 * is on. Standards-bound commands are independently mandatory: they are added
 * even when global Verification is off, and a missing/disabled required ID is
 * returned as an explicit gate failure rather than silently skipped.
 */
export function planVerificationCommands(
  config: VerifyConfig,
  includeConfiguredCommands: boolean,
  requiredCommandIds: readonly string[] = [],
): VerificationCommandPlan {
  const requiredIds = [...new Set(requiredCommandIds.map((id) => id.trim()).filter(Boolean))];
  const byId = new Map(config.commands.map((command) => [command.id, command]));
  const missingRequiredIds = requiredIds.filter((id) => {
    const command = byId.get(id);
    return !command || !command.enabled;
  });

  const commands: VerifyCommand[] = includeConfiguredCommands
    ? config.commands.filter((command) => command.enabled)
    : [];
  const selectedIds = new Set(commands.map((command) => command.id));
  for (const id of requiredIds) {
    const command = byId.get(id);
    if (!command?.enabled || selectedIds.has(id)) continue;
    commands.push(command);
    selectedIds.add(id);
  }

  return { commands, missingRequiredIds };
}
