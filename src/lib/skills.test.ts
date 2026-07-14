import { describe, expect, it } from "vitest";
import type { ActivePluginRuntimeSnapshot } from "./ecosystemClient";
import {
  MAX_PACKAGE_RULES_PER_TURN,
  composeSkillSystemPrompt,
  packageAssistantSkills,
  packageRuleInvocations,
  parseSkillTurn,
  skillCommandMap,
  type SlashSkill,
} from "./skills";

function skill(command: string, id = command): SlashSkill {
  return {
    id,
    source: "local",
    command,
    name: command,
    instructions: `Do ${command}`,
    version: "1",
    contentSha256: `hash-${id}`,
    permissions: [],
  };
}

describe("skill slash invocation", () => {
  it("preserves arguments and freezes several leading skills", () => {
    const parsed = parseSkillTurn("  /review /verify src/auth", [skill("review"), skill("verify")]);
    expect(parsed?.request).toBe("src/auth");
    expect(parsed?.invocations.map((entry) => entry.skill.command)).toEqual(["review", "verify"]);
    expect(parsed?.invocations.every((entry) => entry.arguments === "src/auth")).toBe(true);
  });

  it("leaves unknown slash paths as ordinary text", () => {
    expect(parseSkillTurn("/tmp/project is large", [skill("review")])).toBeNull();
  });

  it("fails closed on collisions and duplicate invocation", () => {
    expect(() => skillCommandMap([skill("review", "a"), skill("review", "b")])).toThrow(/ambiguous/);
    expect(() => parseSkillTurn("/review /review code", [skill("review")])).toThrow(/only be invoked once/);
  });

  it("composes provenance and the normal permission boundary", () => {
    const parsed = parseSkillTurn("/review file.ts", [skill("review")])!;
    const prompt = composeSkillSystemPrompt("BASE", parsed.invocations);
    expect(prompt).toContain("BASE");
    expect(prompt).toContain("Do review");
    expect(prompt).toContain("never bypass");
    expect(prompt).toContain("file.ts");
  });

  it("freezes enabled package rules with version, file, bundle, and permission provenance", () => {
    const snapshot: ActivePluginRuntimeSnapshot = {
      package_id: "com.example.guardrails",
      version: "2.1.0",
      bundle_sha256: "b".repeat(64),
      manifest: {
        schema_version: 1,
        package_id: "com.example.guardrails",
        version: "2.1.0",
        kind: "collection",
        display_name: "Project guardrails",
        description: "Rules",
        content: [{ kind: "rule", path: "rules/project.md", media_type: "text/markdown", sha256: "c".repeat(64) }],
        permissions: [{ permission_id: "read", kind: "read_files", scope: "workspace", reason: "Read project files" }],
        mcp_requirements: [],
        provenance: { publisher: "Example", source: {}, source_revision: "rev", build_reproducible: true },
      },
      text_content: { "rules/project.md": "Follow the repository conventions." },
    };

    const invocations = packageRuleInvocations([snapshot], "change auth.ts");
    expect(invocations).toHaveLength(1);
    expect(invocations[0]).toMatchObject({
      activation: "enabled_package_rule",
      arguments: "change auth.ts",
      skill: {
        source: "package",
        version: "2.1.0",
        contentSha256: "c".repeat(64),
        bundleSha256: "b".repeat(64),
        permissions: [{ permission_id: "read", kind: "read_files", scope: "workspace", reason: "Read project files" }],
      },
    });

    const prompt = composeSkillSystemPrompt("BASE", invocations);
    expect(prompt).toContain("## Enabled package rules");
    expect(prompt).toContain("Follow the repository conventions.");
    expect(prompt).toContain("version 2.1.0 hash");
    expect(prompt).toContain(`Frozen package bundle hash: ${"b".repeat(64)}`);
    expect(prompt).toContain("read_files:workspace");
    expect(prompt).toContain("never grant or expand permissions");
    expect(prompt).not.toContain("## Explicitly invoked skills");
  });

  it("fails closed when enabled package rules exceed the per-turn count", () => {
    const content = Array.from({ length: MAX_PACKAGE_RULES_PER_TURN + 1 }, (_, index) => ({
      kind: "rule" as const,
      path: `rules/${index}.md`,
      media_type: "text/markdown",
      sha256: `${index.toString(16).padStart(2, "0")}${"d".repeat(62)}`,
    }));
    const snapshot = {
      package_id: "com.example.too-many",
      version: "1.0.0",
      bundle_sha256: "a".repeat(64),
      manifest: {
        schema_version: 1,
        package_id: "com.example.too-many",
        version: "1.0.0",
        kind: "collection",
        display_name: "Too many rules",
        description: "Fixture",
        content,
        permissions: [],
        mcp_requirements: [],
        provenance: { publisher: "Example", source: {}, source_revision: "rev", build_reproducible: true },
      },
      text_content: Object.fromEntries(content.map((entry) => [entry.path, "rule"])),
    } satisfies ActivePluginRuntimeSnapshot;
    expect(() => packageRuleInvocations([snapshot], "request")).toThrow(/more than 20 package rules/);
  });

  it("rejects inconsistent or incomplete runtime provenance", () => {
    const snapshot = {
      package_id: "com.example.rules",
      version: "1.0.0",
      bundle_sha256: "a".repeat(64),
      manifest: {
        schema_version: 1,
        package_id: "com.example.other",
        version: "1.0.0",
        kind: "collection",
        display_name: "Rules",
        description: "Fixture",
        content: [],
        permissions: [],
        mcp_requirements: [],
        provenance: { publisher: "Example", source: {}, source_revision: "rev", build_reproducible: true },
      },
      text_content: {},
    } satisfies ActivePluginRuntimeSnapshot;
    expect(() => packageRuleInvocations([snapshot], "request")).toThrow(/inconsistent package provenance/);
  });

  it("keeps assistant persona and rules explicit behind a selectable slash command", () => {
    const snapshot = {
      package_id: "com.example.reviewer",
      version: "3.0.0",
      bundle_sha256: "f".repeat(64),
      manifest: {
        schema_version: 1,
        package_id: "com.example.reviewer",
        version: "3.0.0",
        kind: "assistant",
        display_name: "Review assistant",
        description: "Reviews changes",
        content: [
          { kind: "persona", path: "persona.md", media_type: "text/markdown", sha256: "1".repeat(64) },
          { kind: "rule", path: "rules/review.md", media_type: "text/markdown", sha256: "2".repeat(64) },
        ],
        assistant: {
          persona_content_path: "persona.md",
          skill_package_ids: [],
          starter_workflow_paths: [],
          knowledge_template_path: null,
        },
        permissions: [],
        mcp_requirements: [],
        provenance: { publisher: "Example", source: {}, source_revision: "rev", build_reproducible: true },
      },
      text_content: {
        "persona.md": "Act as a focused reviewer.",
        "rules/review.md": "Report concrete defects first.",
      },
    } satisfies ActivePluginRuntimeSnapshot;

    expect(packageRuleInvocations([snapshot], "review auth.ts")).toEqual([]);
    expect(packageAssistantSkills([snapshot], new Set())).toEqual([]);
    const assistants = packageAssistantSkills([snapshot], new Set([snapshot.package_id]));
    expect(assistants).toHaveLength(1);
    expect(assistants[0].command).toMatch(/^assistant-/);
    expect(assistants[0].instructions).toContain("Act as a focused reviewer.");
    expect(assistants[0].instructions).toContain("Report concrete defects first.");
    expect(assistants[0].description).toContain("saved chat persona is unchanged");

    const parsed = parseSkillTurn(`/${assistants[0].command} auth.ts`, assistants)!;
    expect(parsed.invocations[0].activation).toBe("explicit");
    const prompt = composeSkillSystemPrompt("BASE PERSONA", parsed.invocations);
    expect(prompt).toContain("## Explicitly invoked skills");
    expect(prompt).not.toContain("## Enabled package rules");
    expect(prompt).toContain("Do not change the saved chat persona");
  });
});
