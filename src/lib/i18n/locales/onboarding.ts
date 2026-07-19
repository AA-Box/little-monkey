/** English source of truth for the First-Run Onboarding wizard
 * (ROADMAP.md Phase 6, "First-Run Onboarding and Use-Case Templates") and
 * its "Restart onboarding" AppMenu row. Every other locale imports this
 * dictionary, spreads it first, then overrides every one of its keys with a
 * real, human-reviewed translation — see `de.ts`/`fr.ts` for the override
 * blocks immediately after their own `...onboardingLocale` spread. Reuses
 * the pre-existing `ModeSelector.mode*Label`/`mode*Description` keys for the
 * Safety defaults step rather than duplicating that copy under a second
 * name — those are already translated for every locale. */
export const onboardingLocale: Record<string, string> = {
  "AppMenu.restartOnboarding": "Restart onboarding",

  "Onboarding.skipButton": "Skip onboarding",
  "Onboarding.backButton": "Back",
  "Onboarding.getStartedButton": "Get started",
  "Onboarding.nextButton": "Next",
  "Onboarding.finishButton": "Finish",

  "Onboarding.welcomeTitle": "Welcome to Little Monkey",
  "Onboarding.welcomeBody1": "Little Monkey is local-first: your conversations, files, and prompts live on this machine, not on a server you don't control.",
  "Onboarding.welcomeBody2": "This short setup helps you pick a model, open a workspace, choose how much it can do without asking, and seed one example for how you plan to use it.",
  "Onboarding.welcomePrivacyNote": "Private by default: nothing leaves this device unless you explicitly connect a cloud model provider. Every step below can be skipped.",

  "Onboarding.modelTitle": "Set up a model",
  "Onboarding.modelIntro": "Pick at least one model to chat with. Skipping the cloud section keeps everything fully local — no cloud credentials are ever requested unless you add one yourself.",
  "Onboarding.modelLocalHeading": "Local llama.cpp",
  "Onboarding.modelOllamaHeading": "Ollama",
  "Onboarding.modelCloudHeading": "Cloud provider (optional, bring your own key)",
  "Onboarding.modelCloudIntro": "Only needed if you want a hosted model alongside your local ones. Add a preset provider's key below, or register a custom OpenAI-compatible endpoint.",
  "Onboarding.modelSkipHint": "You can always add or change a model later from Settings.",

  "Onboarding.workspaceTitle": "Open a workspace",
  "Onboarding.workspaceIntro": "Attach the folder you want to work in. The agent can only read, write, and run commands inside an attached workspace.",
  "Onboarding.workspaceCurrentLabel": "Attached: {{path}}",
  "Onboarding.workspaceNoneLabel": "No workspace attached yet",
  "Onboarding.workspaceOpenFolderButton": "Open folder…",
  "Onboarding.workspaceOpeningButton": "Opening…",
  "Onboarding.workspaceRecentHeading": "Recent",
  "Onboarding.workspaceSkipHint": "You can attach or switch a workspace later from the bar above the chat input.",

  "Onboarding.safetyTitle": "Choose a safety default",
  "Onboarding.safetyIntro": "This controls what the agent can do without asking you first. You can change it any time from the mode selector in chat — including the most permissive \"Bypass\" mode, which isn't offered here.",
  "Onboarding.safetyAdvancedNote": "Advanced modes, including Bypass, stay available later from the mode selector in chat.",

  "Onboarding.templateTitle": "Pick a starting template",
  "Onboarding.templateIntro": "Choosing a template seeds a real, editable persona (and, where it fits, a recipe or a disabled scheduled task) so there's something concrete to look at instead of a blank chat. Nothing here is required — you can skip and start from scratch.",
  "Onboarding.templateSeeding": "Seeding…",
  "Onboarding.templateSeededPersona": "Added a persona.",
  "Onboarding.templateSeededRecipe": "Added a recipe.",
  "Onboarding.templateSeededAutomation": "Added a disabled scheduled task — review and enable it in Scheduled Tasks.",

  "Onboarding.assumptionModelLabel": "Model",
  "Onboarding.assumptionModelDescription": "Needs any chat-capable model selected — local, Ollama, or a connected cloud provider.",
  "Onboarding.assumptionToolLabel": "Tool",
  "Onboarding.assumptionToolDescription": "Expects file, shell, web, or browser tools to be available for the model to use.",
  "Onboarding.assumptionConnectorLabel": "Connector",
  "Onboarding.assumptionConnectorDescription": "Would use a live external-service connector (e.g. Jira, Slack) once one ships.",
  "Onboarding.assumptionPermissionLabel": "Permission",
  "Onboarding.assumptionPermissionDescription": "Will trigger file-write or shell permission prompts under your active safety mode.",
  "Onboarding.assumptionVerificationLabel": "Verification",
  "Onboarding.assumptionVerificationDescription": "Works best with this workspace's verification commands configured (Automation settings).",

  "Onboarding.templateCodeReviewName": "Code review",
  "Onboarding.templateCodeReviewDescription": "Reviews the workspace's current diff for correctness and simplification opportunities.",
  "Onboarding.templateResearchName": "Research",
  "Onboarding.templateResearchDescription": "Gathers and cross-checks sources into a short, structured brief.",
  "Onboarding.templateDocsName": "Docs",
  "Onboarding.templateDocsDescription": "Keeps README and docs honest against the current code.",
  "Onboarding.templateQaName": "QA",
  "Onboarding.templateQaDescription": "Runs checks and triages failures without attempting fixes.",
  "Onboarding.templateReleaseName": "Release",
  "Onboarding.templateReleaseDescription": "Drafts release notes and a pre-release checklist from recent commits.",
  "Onboarding.templateHomelabAdminName": "Homelab admin",
  "Onboarding.templateHomelabAdminDescription": "Checks basic health of services this machine runs.",
  "Onboarding.templateJiraTriageName": "Jira triage",
  "Onboarding.templateJiraTriageDescription": "Classifies issue text by severity and type and suggests next steps.",
  "Onboarding.templateJiraTriageCaveat": "No Jira connector is wired in this build — seeds a persona for pasted-in issue text only.",
  "Onboarding.templateSlackSummaryName": "Slack summary",
  "Onboarding.templateSlackSummaryDescription": "Summarizes a chat transcript into decisions, questions, and action items.",
  "Onboarding.templateSlackSummaryCaveat": "No Slack connector is wired in this build — seeds a persona for pasted-in transcripts only.",
  "Onboarding.templateModelEvaluationName": "Model evaluation",
  "Onboarding.templateModelEvaluationDescription": "Compares candidate model outputs on a prompt you provide.",
  "Onboarding.templateModelEvaluationCaveat": "No automated eval-suite system ships yet — seeds a persona for one-off manual comparisons only.",
  "Onboarding.templateBrowserQaName": "Browser QA",
  "Onboarding.templateBrowserQaDescription": "Walks through a web flow using the browser tools and reports what broke.",
  "Onboarding.templateBrowserQaCaveat": "Seeds a persona only — pair it with the Browser Workbench for recorded flows.",

  "Onboarding.finishTitle": "You're set up",
  "Onboarding.finishBody": "That's everything this wizard covers. Every choice you made is editable later from Settings, and this wizard is always one click away from the app menu's \"Restart onboarding\" row.",
  "Onboarding.finishTemplateSummary": "Your \"{{template}}\" starter content is ready to look at in Prompts (and Automation, if it seeded a recipe).",
  "Onboarding.finishReopenHint": "Reopen this wizard any time from the app menu's \"Restart onboarding\" row.",
};
