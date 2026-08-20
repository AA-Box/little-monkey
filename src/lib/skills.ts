import type {
  ActivePluginRuntimeSnapshot,
  ActiveSkillDescriptor,
  PackagePermission,
} from "./ecosystemClient";
import type { PromptEntry } from "../store/promptStore";
import type { EffectivenessRecord } from "./skillLearningClient";
import {
  skillActivationIsPinned,
  skillActivationPolicyFor,
  skillActivationPolicyKey,
  useSkillActivationPolicyStore,
  type SkillActivationPolicy,
} from "../store/skillActivationPolicyStore";

function nativeSkillPolicyIdentity(source: import("./nativeSkillsClient").NativeSkillSource): string {
  if (source.kind === "global") return "global";
  if (source.kind === "workspace") return source.path;
  return `signed-package:${source.package_id}`;
}

export const MAX_SKILLS_PER_TURN = 5;
export const MAX_MODEL_SKILLS = 10;
export const MAX_SKILL_SEARCH_RESULTS = 20;
export const MAX_PACKAGE_RULES_PER_TURN = 20;
export const MAX_PACKAGE_RULE_BYTES_PER_TURN = 64 * 1024;
export const MAX_PACKAGE_ASSISTANT_BYTES = 64 * 1024;

export interface SlashSkill {
  id: string;
  source: "local" | "native" | "package";
  command: string;
  name: string;
  description?: string;
  instructions: string;
  version: string;
  contentSha256: string;
  bundleSha256?: string;
  permissions: PackagePermission[];
  /** How the model may discover and load this skill. Explicit `/command`
   * invocations always remain available as the user's approval. */
  activationPolicy?: SkillActivationPolicy;
  /** Tool names this skill restricts the model to while active — only ever
   * populated for `source: "native"` (ecosystem `SKILL.md`) skills, which
   * are the only ones with an `allowed-tools` frontmatter concept. Empty or
   * absent means unrestricted. */
  allowedTools?: string[];
  /** Bundled file paths (relative to the skill folder, excluding `SKILL.md`)
   * readable on demand via the `read_skill_resource` tool — progressive
   * disclosure, so their contents are never loaded until asked for. Only
   * ever populated for `source: "native"` skills. */
  resourceFiles?: string[];
  /** Stable backend policy identity, also used for ranking pins. */
  policyKey?: string;
  /** Native source path, used for workspace-aware ranking. */
  sourcePath?: string;
}

function catalogTokens(value: string): string[] {
  return value.toLocaleLowerCase().match(/[\p{L}\p{N}][\p{L}\p{N}_-]*/gu) ?? [];
}

function normalizeRankingPath(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/+$/, "");
  // Windows drive and UNC paths are case-insensitive; preserve case for
  // ordinary Unix paths, where it is significant.
  return (/^[A-Za-z]:\//.test(normalized) || normalized.startsWith("//"))
    ? normalized.toLowerCase()
    : normalized;
}

function isWithinWorkspace(sourcePath: string, workspaceRoot: string): boolean {
  const source = normalizeRankingPath(sourcePath);
  const root = normalizeRankingPath(workspaceRoot);
  return source === root || source.startsWith(`${root}/`);
}

export interface SkillRankingSignals {
  pinned?: boolean;
  workspaceRelevant?: boolean;
  verifiedSuccesses?: number;
  recentSuccesses?: number;
  failures?: number;
  corrections?: number;
  lastSuccessfulAtUnixMs?: number;
}

function skillCatalogScore(
  skill: SlashSkill,
  requestText: string,
  signals: ReadonlyMap<string, SkillRankingSignals> = new Map(),
): number {
  const query = requestText.trim().toLowerCase();
  const ranking = signals.get(skill.id) ?? {};
  const command = skill.command.toLowerCase();
  const name = skill.name.toLowerCase();
  const description = (skill.description ?? "").toLowerCase();
  const searchable = `${name} ${description}`;
  let score = query.includes(`/${command}`) ? 2_000 : 0;
  if (ranking.pinned) score += 350;
  if (ranking.workspaceRelevant) score += 160;
  score += Math.min(ranking.verifiedSuccesses ?? 0, 5) * 60;
  score += Math.min(ranking.recentSuccesses ?? 0, 5) * 35;
  score -= Math.min(ranking.failures ?? 0, 5) * 80;
  score -= Math.min(ranking.corrections ?? 0, 3) * 120;
  if (ranking.lastSuccessfulAtUnixMs) {
    const ageDays = Math.max(0, (Date.now() - ranking.lastSuccessfulAtUnixMs) / 86_400_000);
    score += Math.max(0, 100 - ageDays * 3);
  }
  for (const token of catalogTokens(query)) {
    if (token === command) score += 500;
    else if (command.startsWith(token)) score += 160;
    else if (command.includes(token)) score += 80;
    if (token === name) score += 350;
    else if (name.includes(token)) score += 120;
    if (searchable.includes(token)) score += 25;
  }
  return score;
}

/** Orders the model-facing catalog by relevance to the current request. */
export function rankSkillCatalog(
  skills: readonly SlashSkill[],
  requestText = "",
  signals: ReadonlyMap<string, SkillRankingSignals> = new Map(),
): SlashSkill[] {
  return skills
    .map((skill, index) => ({ skill, index, score: skillCatalogScore(skill, requestText, signals) }))
    .sort((left, right) => right.score - left.score || left.skill.command.localeCompare(right.skill.command) || left.index - right.index)
    .map(({ skill }) => skill);
}

/** Converts the durable effectiveness ledger into the same deterministic
 * signal map used by both the initial catalog and fallback search. */
export function skillRankingSignalsFor(
  skills: readonly SlashSkill[],
  records: readonly EffectivenessRecord[],
  workspaceRoot = "",
): Map<string, SkillRankingSignals> {
  const signals = new Map<string, SkillRankingSignals>();
  for (const skill of skills) {
    const matching = records.filter((record) =>
      record.command.toLowerCase() === skill.command.toLowerCase()
      && record.skill_sha256 === skill.contentSha256,
    );
    const verifiedSuccesses = matching.filter((record) => record.outcome === "success" && record.verification_passed === true);
    const failures = matching.filter((record) => record.outcome === "failure" || record.verification_passed === false);
    const lastSuccessfulAtUnixMs = verifiedSuccesses.reduce(
      (latest, record) => Math.max(latest, record.recorded_at_unix_ms),
      0,
    );
    signals.set(skill.id, {
      pinned: skill.policyKey ? skillActivationIsPinned(skill.policyKey) : false,
      workspaceRelevant: Boolean(
        skill.sourcePath
        && workspaceRoot
        && isWithinWorkspace(skill.sourcePath, workspaceRoot),
      ),
      verifiedSuccesses: verifiedSuccesses.length,
      recentSuccesses: verifiedSuccesses.filter((record) => Date.now() - record.recorded_at_unix_ms <= 30 * 86_400_000).length,
      failures: failures.length,
      corrections: matching.filter((record) => record.user_corrected).length,
      lastSuccessfulAtUnixMs: lastSuccessfulAtUnixMs || undefined,
    });
  }
  return signals;
}

export function nativeSkills(entries: import("./nativeSkillsClient").NativeSkillDescriptor[]): SlashSkill[] {
  return entries
    .filter((entry) => entry.source.kind !== "signed_package" && entry.enabled && entry.eligibility.eligible)
    .map((entry) => ({
      id: `native:${entry.source.kind}:${entry.command}:${entry.sha256}`,
      source: "native" as const,
      command: entry.command,
      name: entry.name,
      description: entry.description,
      instructions: entry.instructions,
      version: entry.version,
      contentSha256: entry.sha256,
      permissions: [],
      activationPolicy: (() => {
        const policyKey = skillActivationPolicyKey("native", entry.command, nativeSkillPolicyIdentity(entry.source));
        const state = useSkillActivationPolicyStore.getState();
        return state.hydrated
          ? skillActivationPolicyFor(policyKey, entry.managed ? "automatic" : "ask")
          : entry.managed ? "automatic" : "ask";
      })(),
      policyKey: skillActivationPolicyKey("native", entry.command, nativeSkillPolicyIdentity(entry.source)),
      sourcePath: entry.source.kind === "signed_package" ? undefined : entry.source.path,
      allowedTools: entry.allowed_tools,
      resourceFiles: entry.resource_files,
    }));
}

export interface SkillInvocationSnapshot {
  skill: SlashSkill;
  arguments: string;
  activation: "explicit" | "enabled_package_rule";
}

export interface ParsedSkillTurn {
  invocations: SkillInvocationSnapshot[];
  request: string;
}

export function localPromptSkills(entries: PromptEntry[]): SlashSkill[] {
  return entries
    .filter((entry) => entry.kind === "skill")
    .map((entry) => ({
      id: entry.id,
      source: "local" as const,
      command: entry.command,
      name: entry.name,
      description: entry.description,
      instructions: entry.content,
      version: `local-${entry.updatedAt}`,
      contentSha256: `local:${entry.id}:${entry.updatedAt}`,
      permissions: [],
      activationPolicy: skillActivationPolicyFor(skillActivationPolicyKey("local", entry.command, entry.id)),
      policyKey: skillActivationPolicyKey("local", entry.command, entry.id),
    }));
}

export function packageSkills(entries: ActiveSkillDescriptor[]): SlashSkill[] {
  return entries.map((entry) => ({
    id: entry.package_id,
    source: "package" as const,
    command: entry.command,
    name: entry.name,
    description: entry.description,
    instructions: entry.instructions,
    version: entry.version,
    contentSha256: entry.content_sha256,
    permissions: entry.permissions,
    activationPolicy: skillActivationPolicyFor(skillActivationPolicyKey("package", entry.command, entry.package_id)),
    policyKey: skillActivationPolicyKey("package", entry.command, entry.package_id),
  }));
}

function assertSha256(value: string, label: string): string {
  if (!/^[a-f0-9]{64}$/i.test(value)) {
    throw new Error(`${label} does not contain a valid SHA-256 digest.`);
  }
  return value.toLowerCase();
}

function packageRuleCommand(packageId: string, path: string): string {
  const slug = `${packageId}-${path}`
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 24);
  return `rule-${slug || "package"}`.slice(0, 32);
}

function stableCommandSuffix(value: string): string {
  let hash = 2_166_136_261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16_777_619);
  }
  return (hash >>> 0).toString(36).padStart(7, "0").slice(-7);
}

function packageAssistantCommand(packageId: string): string {
  const slug = packageId
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 13) || "package";
  return `assistant-${slug}-${stableCommandSuffix(packageId)}`;
}

function validatedBundleSha256(snapshot: ActivePluginRuntimeSnapshot): string {
  if (snapshot.manifest.package_id !== snapshot.package_id || snapshot.manifest.version !== snapshot.version) {
    throw new Error(`Enabled plugin ${snapshot.package_id} returned inconsistent package provenance.`);
  }
  return assertSha256(snapshot.bundle_sha256, `Enabled plugin ${snapshot.package_id}`);
}

/**
 * Builds explicit, turn-scoped assistant commands. Assistant packages never
 * replace the session persona implicitly; selecting this command snapshots
 * the package's persona/instruction/rule text into the ordinary skill prompt.
 * Assistants with disabled declared skill packages stay unavailable until
 * their dependencies are enabled (the Plugin runtime panel explains setup).
 */
export function packageAssistantSkills(
  snapshots: readonly ActivePluginRuntimeSnapshot[],
  readyPackageIds: ReadonlySet<string>,
): SlashSkill[] {
  const activePackageIds = new Set(snapshots.map((snapshot) => snapshot.package_id));
  const assistants: SlashSkill[] = [];
  for (const snapshot of [...snapshots].sort((left, right) => left.package_id.localeCompare(right.package_id))) {
    if (snapshot.manifest.kind !== "assistant") continue;
    if (!readyPackageIds.has(snapshot.package_id)) continue;
    const assistant = snapshot.manifest.assistant;
    if (!assistant) {
      throw new Error(`Enabled assistant ${snapshot.package_id} has no assistant composition.`);
    }
    if (assistant.skill_package_ids.some((packageId) => !activePackageIds.has(packageId))) continue;
    const bundleSha256 = validatedBundleSha256(snapshot);
    const persona = snapshot.manifest.content.find(
      (reference) => reference.kind === "persona" && reference.path === assistant.persona_content_path,
    );
    if (!persona) {
      throw new Error(`Enabled assistant ${snapshot.package_id} is missing its declared persona reference.`);
    }
    const references = snapshot.manifest.content
      .filter((reference) => (
        reference.kind === "persona"
          ? reference.path === assistant.persona_content_path
          : ["instructions", "prompt", "rule"].includes(reference.kind)
      ))
      .slice()
      .sort((left, right) => {
        if (left.path === assistant.persona_content_path) return -1;
        if (right.path === assistant.persona_content_path) return 1;
        return left.path.localeCompare(right.path);
      });
    let totalBytes = 0;
    const instructionBlocks = references.map((reference) => {
      const content = snapshot.text_content[reference.path];
      if (typeof content !== "string") {
        throw new Error(`Enabled assistant ${snapshot.package_id} is missing verified content ${reference.path}.`);
      }
      totalBytes += new TextEncoder().encode(content).byteLength;
      if (totalBytes > MAX_PACKAGE_ASSISTANT_BYTES) {
        throw new Error(
          `Enabled assistant ${snapshot.package_id} exceeds the ${MAX_PACKAGE_ASSISTANT_BYTES.toLocaleString()}-byte turn instruction limit.`,
        );
      }
      const digest = assertSha256(reference.sha256, `Enabled assistant content ${snapshot.package_id}:${reference.path}`);
      return `#### ${reference.kind} · ${reference.path} · sha256:${digest}\n${content}`;
    });
    assistants.push({
      id: `package-assistant:${snapshot.package_id}`,
      source: "package",
      command: packageAssistantCommand(snapshot.package_id),
      name: snapshot.manifest.display_name,
      description: `${snapshot.manifest.description} Explicit for one turn; the saved chat persona is unchanged.`,
      activationPolicy: "manual",
      instructions: [
        "Use this explicitly selected package assistant for the current turn only. Do not change the saved chat persona. Do not auto-run starter workflows.",
        ...instructionBlocks,
      ].join("\n\n"),
      version: snapshot.version,
      contentSha256: bundleSha256,
      bundleSha256,
      permissions: structuredClone(snapshot.manifest.permissions),
    });
  }
  return assistants;
}

/**
 * Converts verified, enabled package Rule content into a bounded immutable
 * turn snapshot. The Rust command reconstructs every entry from its
 * checksum-validated active bundle, while this boundary also rejects
 * inconsistent provenance instead of silently dropping a package rule.
 */
export function packageRuleInvocations(
  snapshots: readonly ActivePluginRuntimeSnapshot[],
  request: string,
): SkillInvocationSnapshot[] {
  const invocations: SkillInvocationSnapshot[] = [];
  let totalBytes = 0;
  const orderedSnapshots = [...snapshots].sort((left, right) => left.package_id.localeCompare(right.package_id));

  for (const snapshot of orderedSnapshots) {
    // Assistant content is opt-in through /assistant-… and must never alter
    // the current persona or its rules merely because a package is enabled.
    if (snapshot.manifest.kind === "assistant") continue;
    const bundleSha256 = validatedBundleSha256(snapshot);
    const rules = snapshot.manifest.content
      .filter((reference) => reference.kind === "rule")
      .slice()
      .sort((left, right) => left.path.localeCompare(right.path));

    for (const reference of rules) {
      if (invocations.length >= MAX_PACKAGE_RULES_PER_TURN) {
        throw new Error(`Enabled plugins declare more than ${MAX_PACKAGE_RULES_PER_TURN} package rules for one turn.`);
      }
      const instructions = snapshot.text_content[reference.path];
      if (typeof instructions !== "string") {
        throw new Error(`Enabled plugin ${snapshot.package_id} is missing verified rule content ${reference.path}.`);
      }
      totalBytes += new TextEncoder().encode(instructions).byteLength;
      if (totalBytes > MAX_PACKAGE_RULE_BYTES_PER_TURN) {
        throw new Error(
          `Enabled package rules exceed the ${MAX_PACKAGE_RULE_BYTES_PER_TURN.toLocaleString()}-byte turn limit. Disable or reduce a rule package before sending.`,
        );
      }
      const contentSha256 = assertSha256(
        reference.sha256,
        `Enabled plugin rule ${snapshot.package_id}:${reference.path}`,
      );
      invocations.push({
        activation: "enabled_package_rule",
        arguments: request,
        skill: {
          id: `package-rule:${snapshot.package_id}:${reference.path}`,
          source: "package",
          command: packageRuleCommand(snapshot.package_id, reference.path),
          name: `${snapshot.manifest.display_name} · ${reference.path}`,
          description: `Enabled declarative rule from ${snapshot.package_id}`,
          instructions,
          version: snapshot.version,
          contentSha256,
          bundleSha256,
          permissions: structuredClone(snapshot.manifest.permissions),
        },
      });
    }
  }

  return invocations;
}

/** Builds a collision-free command registry. Ambiguity fails closed instead
 * of silently choosing a local or marketplace skill with the same command. */
export function skillCommandMap(skills: SlashSkill[]): Map<string, SlashSkill> {
  const registry = new Map<string, SlashSkill>();
  for (const skill of skills) {
    const command = skill.command.toLowerCase();
    const existing = registry.get(command);
    if (existing && existing.id !== skill.id) {
      throw new Error(`Skill command /${command} is ambiguous between ${existing.name} and ${skill.name}.`);
    }
    registry.set(command, skill);
  }
  return registry;
}

/** Parses only explicitly installed skills at the beginning of a message.
 * Unknown leading `/text` remains ordinary chat text (important for paths),
 * while several known commands may be stacked before one shared request. */
export function parseSkillTurn(text: string, skills: SlashSkill[]): ParsedSkillTurn | null {
  const registry = skillCommandMap(skills);
  let cursor = text.search(/\S/);
  if (cursor < 0 || text[cursor] !== "/") return null;

  const selected: SlashSkill[] = [];
  while (text[cursor] === "/") {
    const tokenEndMatch = text.slice(cursor).search(/\s/);
    const end = tokenEndMatch < 0 ? text.length : cursor + tokenEndMatch;
    const command = text.slice(cursor + 1, end).toLowerCase();
    if (!/^[a-z0-9][a-z0-9-]{0,31}$/.test(command)) break;
    const skill = registry.get(command);
    if (!skill) break;
    if (selected.some((entry) => entry.id === skill.id)) {
      throw new Error(`Skill /${command} can only be invoked once per turn.`);
    }
    selected.push(skill);
    if (selected.length > MAX_SKILLS_PER_TURN) {
      throw new Error(`A turn can invoke at most ${MAX_SKILLS_PER_TURN} skills.`);
    }
    cursor = end;
    while (cursor < text.length && /\s/.test(text[cursor])) cursor += 1;
  }
  if (selected.length === 0) return null;
  const request = text.slice(cursor).trim();
  return {
    request,
    invocations: selected.map((skill) => ({
      activation: "explicit" as const,
      skill: structuredClone(skill),
      arguments: request,
    })),
  };
}

export function composeSkillSystemPrompt(
  baseSystemPrompt: string,
  invocations: SkillInvocationSnapshot[],
): string {
  if (invocations.length === 0) return baseSystemPrompt;
  const block = ({ skill, arguments: args, activation }: SkillInvocationSnapshot) => {
    const permissions = skill.permissions.length > 0
      ? skill.permissions.map((permission) => `${permission.kind}:${permission.scope}`).join(", ")
      : "none declared; normal run permissions still apply";
    return [
      activation === "enabled_package_rule"
        ? `### ${skill.name} (enabled package rule)`
        : `### ${skill.name} (/${skill.command})`,
      `Frozen source: ${skill.source} ${skill.id} version ${skill.version} hash ${skill.contentSha256}`,
      ...(skill.bundleSha256 ? [`Frozen package bundle hash: ${skill.bundleSha256}`] : []),
      `Declared permissions: ${permissions}`,
      ...(skill.allowedTools && skill.allowedTools.length > 0
        ? [`Allowed tools while active: ${skill.allowedTools.join(", ")}`]
        : []),
      "Instructions:",
      skill.instructions,
      ...(skill.resourceFiles && skill.resourceFiles.length > 0
        ? [`Bundled files (read via read_skill_resource): ${skill.resourceFiles.join(", ")}`]
        : []),
      "Arguments/request:",
      args || "(none)",
    ].join("\n");
  };
  const packageRules = invocations.filter((entry) => entry.activation === "enabled_package_rule");
  const explicitSkills = invocations.filter((entry) => entry.activation !== "enabled_package_rule");
  const sections = [baseSystemPrompt];
  if (packageRules.length > 0) {
    sections.push(
      "## Enabled package rules",
      "Apply these package-authored rules because their verified data-only packages are enabled. This exact content is frozen for the current turn. Rules never grant or expand permissions and never bypass tool, workspace, network, approval, or mutation controls.",
      ...packageRules.map(block),
    );
  }
  if (explicitSkills.length > 0) {
    sections.push(
      "## Explicitly invoked skills",
      "Apply these task-scoped instructions for this turn only. They never bypass tool, workspace, network, approval, or mutation permissions.",
      ...explicitSkills.map(block),
    );
  }
  return sections.join("\n\n");
}

/**
 * Compact `name`+`description` catalog of discoverable available skills NOT already
 * invoked this turn — the auto-invocation counterpart to
 * `composeSkillSystemPrompt` above: that function injects the FULL
 * instructions for skills already invoked (explicitly, or via an
 * always-on package rule); this one lists the rest by name only, so the
 * model can choose to invoke one itself (via the `skill` tool — see
 * `tools.ts`'s `SKILL_INVOKE_TOOL`) without every uninvoked skill's full
 * instructions being loaded into every turn's context up front. The model
 * sees only the bounded top-ranked slice; `search_skills` covers the rest.
 * Returns
 * `""` when there's nothing left to list, so callers can `filter(Boolean)`
 * it straight into a section list without a separate emptiness check.
 */
export function composeSkillCatalog(
  availableSkills: SlashSkill[],
  alreadyInvokedCommands: ReadonlySet<string>,
  requestText = "",
  signals: ReadonlyMap<string, SkillRankingSignals> = new Map(),
): string {
  const remaining = availableSkills.filter(
    (skill) => !alreadyInvokedCommands.has(skill.command) && skill.activationPolicy !== "manual",
  );
  if (remaining.length === 0) return "";
  const ranked = rankSkillCatalog(remaining, requestText, signals);
  const visible = ranked.slice(0, MAX_MODEL_SKILLS);
  return [
    "## Available skills",
    "These skills are ranked by relevance. Automatic skills may be loaded immediately with the `skill` tool. Ask skills may also be requested with the `skill` tool; Little Monkey will pause and ask the user before their instructions are loaded. Manual skills are available only through explicit /command invocation.",
    ...visible.map((skill) => `- /${skill.command} — ${skill.description ?? skill.name} [policy: ${skill.activationPolicy ?? "automatic"}]`),
    ...(ranked.length > visible.length
      ? ["More skills are available. Use the `search_skills` tool when the relevant skill is not listed here."]
      : []),
  ].join("\n");
}

/** Compact fallback search over the full installed registry. It never returns
 * instructions, and manual-only skills are intentionally undiscoverable. */
export function formatSkillSearchResults(
  availableSkills: readonly SlashSkill[],
  alreadyInvokedCommands: ReadonlySet<string>,
  query: string,
  signals: ReadonlyMap<string, SkillRankingSignals> = new Map(),
): string {
  const tokens = catalogTokens(query);
  const candidates = availableSkills.filter(
    (skill) => !alreadyInvokedCommands.has(skill.command) && skill.activationPolicy !== "manual",
  );
  const matches = rankSkillCatalog(candidates, query, signals)
    .filter((skill) => {
      const searchable = `${skill.command} ${skill.name} ${skill.description ?? ""}`.toLowerCase();
      return query.toLowerCase().includes(`/${skill.command.toLowerCase()}`)
        || tokens.some((token) => searchable.includes(token));
    })
    .slice(0, MAX_SKILL_SEARCH_RESULTS);
  return JSON.stringify({
    query,
    results: matches.map((skill) => ({
      command: `/${skill.command}`,
      name: skill.name,
      description: skill.description ?? skill.name,
      policy: skill.activationPolicy ?? "automatic",
    })),
  });
}

/**
 * Formats a model-invoked (`skill` tool call) skill's instructions as the
 * tool RESULT content — the auto-invocation counterpart to
 * `composeSkillSystemPrompt`'s per-skill `block()` (see `turnEngine.ts`'s
 * `executeToolCall`, the `name === 'skill'` branch that calls this). Kept as
 * its own small function rather than sharing `block()` directly: that
 * closure is local to `composeSkillSystemPrompt` and formats a whole
 * system-prompt SECTION (with a `###` header and "never bypass..." framing
 * appropriate to a few-times-per-turn injected block), whereas this formats
 * a single tool result the model reads once, right after asking for it — the
 * shape is similar by design (same instructions/allowed-tools/resource-files
 * fields) but the two are edited independently on purpose.
 */
export function formatSkillToolResult(skill: SlashSkill, argumentsText: string): string {
  return [
    `Skill: ${skill.name} (/${skill.command})`,
    ...(skill.allowedTools && skill.allowedTools.length > 0
      ? [`Allowed tools while active: ${skill.allowedTools.join(", ")}`]
      : []),
    "Instructions:",
    skill.instructions,
    ...(skill.resourceFiles && skill.resourceFiles.length > 0
      ? [`Bundled files (read via read_skill_resource): ${skill.resourceFiles.join(", ")}`]
      : []),
    ...(argumentsText ? ["Arguments/request:", argumentsText] : []),
  ].join("\n");
}
