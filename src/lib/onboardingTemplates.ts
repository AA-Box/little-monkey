import { findByCommand, usePromptStore } from "../store/promptStore";
import { useRecipeStore } from "../store/recipeStore";
import { useAutomationsStore } from "../store/automationsStore";

/**
 * The five kinds of "this template assumes something" declaration the
 * roadmap asks the picker to be explicit about. Purely descriptive labels
 * shown as chips on each template card (see `OnboardingWizard.tsx`) — they
 * never gate anything themselves.
 *  - "model": needs any chat-capable model selected (local, Ollama, or a
 *    cloud provider) — true of every template.
 *  - "tool": the persona expects file/shell/web/browser tools to be
 *    available (i.e. not running in a tools-off surface like Compare).
 *  - "connector": needs a live external-service connector (Jira, Slack,
 *    etc.). No connector system ships in this build yet, so every template
 *    that would naturally want one instead ships a `caveatKey` explaining
 *    that only the persona seeds today.
 *  - "permission": the persona will trigger file-write/shell permission
 *    prompts under the active `PermissionMode` — worth calling out right
 *    after the Safety defaults step.
 *  - "verification": expects the workspace's configured verification
 *    commands (`verifyStore.ts` / src-tauri/src/verify.rs) to be set up.
 */
export type OnboardingAssumptionKind = "model" | "tool" | "connector" | "permission" | "verification";

/** One seed action `seedOnboardingTemplate` may perform, and whether it
 * actually happened — surfaced back to the wizard so it can render an
 * honest "here's what was created" confirmation rather than assuming every
 * seed kind a template declares always succeeds (recipe/automation seeding
 * calls into Tauri `invoke`, which can fail or simply not exist outside the
 * Tauri shell). */
export interface OnboardingSeedResult {
  personaCreated: boolean;
  recipeCreated: boolean;
  automationCreated: boolean;
  /** Set when a recipe/automation seed step threw — the persona seed itself
   * never throws, so this only ever reflects that part. */
  error: string | null;
}

/** A curated use-case template shown on the wizard's picker step. `seeds`
 * declares which of {persona, recipe, automation} `seedOnboardingTemplate`
 * will attempt for this id — every template seeds a persona; only the ones
 * with a genuine recipe-shaped workflow also seed a recipe, and only the two
 * that are naturally periodic also seed a (disabled-by-default) scheduled
 * task on top of that recipe. Model-routing rules remain a roadmap item;
 * automated eval suites live in the Workflow & Agent Test Harness, while
 * onboarding deliberately seeds only lightweight starter personas here.
 */
export interface OnboardingTemplate {
  id: string;
  nameKey: string;
  descriptionKey: string;
  assumptions: OnboardingAssumptionKind[];
  /** Extra one-liner shown under the assumption chips for templates that
   * need a connector, another feature surface, or a deliberate setup step. */
  caveatKey?: string;
  seeds: {
    persona: true;
    recipe?: boolean;
    automation?: boolean;
  };
}

export const ONBOARDING_TEMPLATES: OnboardingTemplate[] = [
  {
    id: "code-review",
    nameKey: "Onboarding.templateCodeReviewName",
    descriptionKey: "Onboarding.templateCodeReviewDescription",
    assumptions: ["model", "tool", "permission"],
    seeds: { persona: true, recipe: true },
  },
  {
    id: "research",
    nameKey: "Onboarding.templateResearchName",
    descriptionKey: "Onboarding.templateResearchDescription",
    assumptions: ["model", "tool"],
    seeds: { persona: true, recipe: true },
  },
  {
    id: "docs",
    nameKey: "Onboarding.templateDocsName",
    descriptionKey: "Onboarding.templateDocsDescription",
    assumptions: ["model", "tool", "permission"],
    seeds: { persona: true, recipe: true },
  },
  {
    id: "qa",
    nameKey: "Onboarding.templateQaName",
    descriptionKey: "Onboarding.templateQaDescription",
    assumptions: ["model", "tool", "permission", "verification"],
    seeds: { persona: true, recipe: true },
  },
  {
    id: "release",
    nameKey: "Onboarding.templateReleaseName",
    descriptionKey: "Onboarding.templateReleaseDescription",
    assumptions: ["model", "tool", "permission"],
    seeds: { persona: true, recipe: true, automation: true },
  },
  {
    id: "homelab-admin",
    nameKey: "Onboarding.templateHomelabAdminName",
    descriptionKey: "Onboarding.templateHomelabAdminDescription",
    assumptions: ["model", "tool", "permission"],
    seeds: { persona: true, recipe: true, automation: true },
  },
  {
    id: "jira-triage",
    nameKey: "Onboarding.templateJiraTriageName",
    descriptionKey: "Onboarding.templateJiraTriageDescription",
    assumptions: ["model"],
    caveatKey: "Onboarding.templateJiraTriageCaveat",
    seeds: { persona: true },
  },
  {
    id: "slack-summary",
    nameKey: "Onboarding.templateSlackSummaryName",
    descriptionKey: "Onboarding.templateSlackSummaryDescription",
    assumptions: ["model"],
    caveatKey: "Onboarding.templateSlackSummaryCaveat",
    seeds: { persona: true },
  },
  {
    id: "model-evaluation",
    nameKey: "Onboarding.templateModelEvaluationName",
    descriptionKey: "Onboarding.templateModelEvaluationDescription",
    assumptions: ["model"],
    caveatKey: "Onboarding.templateModelEvaluationCaveat",
    seeds: { persona: true },
  },
  {
    id: "browser-qa",
    nameKey: "Onboarding.templateBrowserQaName",
    descriptionKey: "Onboarding.templateBrowserQaDescription",
    assumptions: ["model", "tool"],
    caveatKey: "Onboarding.templateBrowserQaCaveat",
    seeds: { persona: true },
  },
];

interface TemplateSeedContent {
  persona: {
    name: string;
    command: string;
    description: string;
    content: string;
  };
  recipeName?: string;
  recipeYaml?: string;
  /** Croner cron expression (see `automationsStore.ts`'s `AutomationEntry.cron`). */
  automationCron?: string;
}

/** A shared, generic-purpose local model reference used by every seeded
 * recipe's `target`. It intentionally names an Ollama tag rather than a
 * cloud provider — recipes only validate their target's *shape* server-side
 * (exactly one of provider/ollama/local_url set — see
 * src-tauri/src/recipes.rs), not that the tag is actually pulled, so this
 * stays meaningful even for a user who skipped the Model setup step
 * entirely; the recipe simply won't run until they pull (or repoint) it. */
const SEED_RECIPE_TARGET_YAML = "target:\n  ollama: qwen2.5:7b\n";

const TEMPLATE_SEED_CONTENT: Record<string, TemplateSeedContent> = {
  "code-review": {
    persona: {
      name: "PR Review Assistant",
      command: "pr-review",
      description: "Reviews the workspace's current diff for correctness, clarity, and simplification.",
      content:
        "You review this workspace's current changes like a meticulous senior engineer. Point out correctness bugs, unclear naming, missed edge cases, and simplification opportunities. Be direct and specific — cite the exact file and line. Prefer small, targeted suggestions over rewrites.",
    },
    recipeName: "onboarding-code-review",
    recipeYaml:
      "version: 1\nname: onboarding-code-review\ndescription: Review the working tree's current changes for correctness and simplification opportunities.\n" +
      SEED_RECIPE_TARGET_YAML +
      "permission_mode: manual\nprompt: |\n  Review the current diff in this workspace. Point out correctness bugs, unclear naming, missed edge cases, and simplification opportunities. Cite exact file/line references. Prefer small, targeted suggestions over rewrites.\n",
  },
  research: {
    persona: {
      name: "Research Analyst",
      command: "research-analyst",
      description: "Gathers and cross-checks sources into a structured brief.",
      content:
        "You are a careful research analyst. Given a topic, use web search and page fetches to gather multiple independent sources, note where they agree and disagree, and produce a short brief: summary, key findings, open questions, and a source list. Never present a single source's claim as settled fact.",
    },
    recipeName: "onboarding-research-brief",
    recipeYaml:
      "version: 1\nname: onboarding-research-brief\ndescription: Produce a structured research brief on a given topic using the web tools.\n" +
      SEED_RECIPE_TARGET_YAML +
      "permission_mode: manual\nprompt: |\n  Research the topic given in the conversation. Use web search and page fetches to gather at least three independent sources, note where they agree and disagree, and produce a short brief: summary, key findings, open questions, and sources.\n",
  },
  docs: {
    persona: {
      name: "Docs Writer",
      command: "docs-writer",
      description: "Keeps README/docs in line with the current code.",
      content:
        "You keep documentation honest. Compare README/docs against the current code, list every place documentation is stale, missing, or contradicts the code, then propose the smallest edit that fixes each one. Prefer plain language and concrete examples over abstract prose.",
    },
    recipeName: "onboarding-docs-update",
    recipeYaml:
      "version: 1\nname: onboarding-docs-update\ndescription: Bring a workspace's README/docs in line with the current code.\n" +
      SEED_RECIPE_TARGET_YAML +
      "permission_mode: manual\nprompt: |\n  Compare this workspace's README and docs against the current code. List every place documentation is stale or missing, then propose the smallest edit that fixes each one.\n",
  },
  qa: {
    persona: {
      name: "QA Engineer",
      command: "qa-engineer",
      description: "Runs checks and triages failures without attempting fixes.",
      content:
        "You are a QA engineer doing a triage pass, not a fix pass. Run this workspace's test suite (or the closest available check), read the output, and summarize every failure with its likely root cause and severity. Do not attempt fixes unless explicitly asked to.",
    },
    recipeName: "onboarding-qa-pass",
    recipeYaml:
      "version: 1\nname: onboarding-qa-pass\ndescription: Run the workspace's test suite and triage failures.\n" +
      SEED_RECIPE_TARGET_YAML +
      "permission_mode: manual\nprompt: |\n  Run this workspace's test suite (or the closest available check), read the output, and summarize every failure with its likely root cause. Do not attempt fixes yet — this is a triage pass.\n",
  },
  release: {
    persona: {
      name: "Release Manager",
      command: "release-manager",
      description: "Drafts release notes and a pre-release checklist.",
      content:
        "You are a release manager. Look at commits since the last tagged release. Draft user-facing release notes grouped by feature/fix/chore, and list any pre-release checklist items (migrations, changelog, version bump) still outstanding. Flag anything that looks like a breaking change.",
    },
    recipeName: "onboarding-release-checklist",
    recipeYaml:
      "version: 1\nname: onboarding-release-checklist\ndescription: Draft release notes and a pre-release checklist from recent commits.\n" +
      SEED_RECIPE_TARGET_YAML +
      "permission_mode: manual\nprompt: |\n  Look at commits since the last tagged release. Draft user-facing release notes grouped by feature/fix/chore, and list any pre-release checklist items (migrations, changelog, version bump) still outstanding.\n",
    // Weekly, Monday 9am — seeded disabled (see `seedOnboardingTemplate`),
    // so nothing actually runs unattended until the user reviews and
    // enables it from the Scheduled Tasks panel.
    automationCron: "0 9 * * 1",
  },
  "homelab-admin": {
    persona: {
      name: "Homelab Admin",
      command: "homelab-admin",
      description: "Checks basic health of locally-run services.",
      content:
        "You help administer a homelab. Check disk space, running containers (if any), and locally listed service ports on this machine. Summarize what's healthy, what's degraded, and what needs attention — be specific about which service and why, not just a generic warning.",
    },
    recipeName: "onboarding-homelab-health-check",
    recipeYaml:
      "version: 1\nname: onboarding-homelab-health-check\ndescription: Check basic health of local services this machine runs.\n" +
      SEED_RECIPE_TARGET_YAML +
      "permission_mode: manual\nprompt: |\n  Check disk space, running Docker containers (if any), and any locally listed service ports on this machine. Summarize what's healthy, what's degraded, and what needs attention.\n",
    // Daily, 8am — seeded disabled, same rationale as "release" above.
    automationCron: "0 8 * * *",
  },
  "jira-triage": {
    persona: {
      name: "Jira Triage Assistant",
      command: "jira-triage",
      description: "Classifies pasted issue text by severity/type and suggests next steps.",
      content:
        "You triage software issues. Given the text of an issue (pasted in, since no live tracker is connected), classify it by severity and type, flag likely duplicates or missing repro steps, and suggest a next step or owner. Keep it to a few lines per issue.",
    },
  },
  "slack-summary": {
    persona: {
      name: "Channel Summarizer",
      command: "slack-digest",
      description: "Summarizes pasted chat transcripts into a short digest.",
      content:
        "You summarize team chat transcripts (pasted in, since no live workspace is connected). Produce a short digest: decisions made, open questions, and action items with an owner where one is named. Skip small talk entirely.",
    },
  },
  "model-evaluation": {
    persona: {
      name: "Model Evaluator",
      command: "model-evaluator",
      description: "Compares model outputs on a prompt you provide, side by side.",
      content:
        "You help evaluate model outputs. Given a prompt and two or more candidate responses, compare them on correctness, completeness, and clarity, then give a short recommendation with reasons. Judge only what's in front of you — don't assume a benchmark result you weren't given.",
    },
  },
  "browser-qa": {
    persona: {
      name: "Browser QA Tester",
      command: "browser-qa",
      description: "Walks through a web flow and reports what broke.",
      content:
        "You perform manual QA on a web flow using the browser tools. Walk through the flow step by step, note the expected vs. actual result at each step, and report exactly where and how it diverges. Prefer precise selectors/screenshots over vague descriptions.",
    },
  },
};

/** Finds `usePromptStore`'s persona/snippet/skill entries that already use a
 * given command, so re-selecting a template on a later wizard visit doesn't
 * pile up duplicate personas. */
function personaAlreadySeeded(command: string): boolean {
  return findByCommand(usePromptStore.getState().entries, command) !== undefined;
}

/**
 * Seeds real, inspectable local content for a chosen use-case template,
 * using only the pre-existing `promptStore`/`recipeStore`/`automationsStore`
 * — never a fabricated subsystem. Always seeds a persona (skipped if a
 * same-command persona already exists, e.g. from a previous visit to this
 * step); additionally seeds a recipe and/or a *disabled* scheduled task for
 * the templates that declare them in `ONBOARDING_TEMPLATES`. The scheduled
 * task is seeded disabled deliberately — mirrors `verifyEnabled`'s "opt-in
 * before anything runs unattended" posture elsewhere in this app — so the
 * user reviews and explicitly enables it from the Scheduled Tasks panel
 * rather than a wizard silently arming a cron job.
 *
 * Recipe/automation seeding calls into Tauri `invoke` (via `recipeStore.save`),
 * which is unavailable in plain-browser dev and can fail for other reasons
 * (e.g. a malformed workspace state) — failures there are caught and
 * reported via `error` rather than thrown, so a template's persona still
 * gets seeded even if its recipe doesn't.
 */
export async function seedOnboardingTemplate(templateId: string): Promise<OnboardingSeedResult> {
  const template = ONBOARDING_TEMPLATES.find((candidate) => candidate.id === templateId);
  const content = TEMPLATE_SEED_CONTENT[templateId];
  const result: OnboardingSeedResult = {
    personaCreated: false,
    recipeCreated: false,
    automationCreated: false,
    error: null,
  };
  if (!template || !content) return result;

  if (!personaAlreadySeeded(content.persona.command)) {
    usePromptStore.getState().addEntry({
      kind: "persona",
      name: content.persona.name,
      command: content.persona.command,
      content: content.persona.content,
      description: content.persona.description,
    });
    result.personaCreated = true;
  }

  if (template.seeds.recipe && content.recipeName && content.recipeYaml) {
    try {
      await useRecipeStore.getState().save(content.recipeName, content.recipeYaml);
      result.recipeCreated = true;
    } catch (err) {
      result.error = err instanceof Error ? err.message : String(err);
    }
  }

  if (result.recipeCreated && template.seeds.automation && content.automationCron && content.recipeName) {
    const alreadyScheduled = useAutomationsStore
      .getState()
      .entries.some((entry) => entry.recipeName === content.recipeName);
    if (!alreadyScheduled) {
      useAutomationsStore.getState().addEntry({
        recipeName: content.recipeName,
        cron: content.automationCron,
        enabled: false,
        catchUpIfMissed: false,
      });
      result.automationCreated = true;
    }
  }

  return result;
}
