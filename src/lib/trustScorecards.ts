/**
 * Trust Scorecards (ROADMAP.md Phase 7, item 28) — pure scoring logic, no
 * React and no `invoke()` calls of its own. Every function here takes
 * already-fetched snapshots from the app's existing stores/clients
 * (`modelStore`, `connectorsStore`, `mcpStore`, `nativeSkillsClient`,
 * `ecosystemStore`, `usageHistoryStore`) and derives a per-dimension trust
 * score for each entity, citing the EXACT field it read to produce that
 * score. Nothing here invents a number: a dimension with no real signal to
 * read is always `"unknown"` ("insufficient evidence"), never a guessed
 * value. `trustScorecardsStore.ts` is the only caller — it owns pulling
 * fresh snapshots from those stores and re-computing on demand.
 */
import type { ModelInfo, OllamaModelInfo, ProviderConfig } from "../store/modelStore";
import type { ConnectorAccount } from "../store/connectorsStore";
import type { McpServerInfo } from "../store/mcpStore";
import type { NativeSkillDescriptor } from "./nativeSkillsClient";
import type {
  PackageCatalogEntry,
  PluginRuntimeDescriptor,
  WorkflowDefinition,
  WorkflowRunHistory,
} from "./ecosystemClient";
import type { ModelUsageTotals } from "../store/usageHistoryStore";

export type TrustEntityKind = "model" | "connector" | "mcp_server" | "skill" | "workflow" | "plugin";

/** `"unknown"` means "insufficient evidence" — the concrete signal this
 * dimension would need simply isn't tracked anywhere yet for this entity,
 * so no score is fabricated. */
export type TrustLevel = "good" | "fair" | "poor" | "unknown";

export type TrustDimensionKey = "quality" | "cost" | "privacy" | "security" | "reliability" | "provenance";

export const TRUST_DIMENSION_KEYS: TrustDimensionKey[] = [
  "quality",
  "cost",
  "privacy",
  "security",
  "reliability",
  "provenance",
];

/** One concrete fact backing a dimension's level — `field` names the exact
 * store/struct field it was read from, so a reader can go verify it. */
export interface TrustEvidenceItem {
  field: string;
  fact: string;
}

export interface TrustDimensionScore {
  level: TrustLevel;
  evidence: TrustEvidenceItem[];
}

export interface TrustScorecard {
  id: string;
  kind: TrustEntityKind;
  name: string;
  subtitle: string | null;
  dimensions: Record<TrustDimensionKey, TrustDimensionScore>;
}

function unknownDim(field: string, fact: string): TrustDimensionScore {
  return { level: "unknown", evidence: [{ field, fact }] };
}

function dim(level: TrustLevel, evidence: TrustEvidenceItem[]): TrustDimensionScore {
  return { level, evidence };
}

/** Ordinal weight used only for sorting/summarizing in the panel — never
 * shown as a fabricated "score number", just used to rank rows so the
 * comparison view can put weaker profiles first. */
export const LEVEL_WEIGHT: Record<TrustLevel, number> = { poor: 0, unknown: 1, fair: 2, good: 3 };

/**
 * Aggregate ordinal used by `TrustScorecardsPanel.tsx` to rank rows within a
 * kind: lower = weaker = shown first, exactly the "weaker profiles first"
 * behavior `LEVEL_WEIGHT`'s doc comment promises. A plain sum (not an
 * average) is fine because every card scores the same six dimensions.
 */
export function scorecardWeight(card: TrustScorecard): number {
  return TRUST_DIMENSION_KEYS.reduce(
    (total, key) => total + LEVEL_WEIGHT[card.dimensions[key]?.level ?? "unknown"],
    0,
  );
}

// ---------------------------------------------------------------------------
// Models — local llama.cpp, Ollama, and configured cloud providers.
// ---------------------------------------------------------------------------

export interface ModelUsageLookup {
  byModel: Record<string, ModelUsageTotals>;
}

function usageEvidence(label: string, usage: ModelUsageLookup): TrustDimensionScore {
  const totals = usage.byModel[label];
  if (!totals) {
    return unknownDim(
      "usageHistoryStore.byModel",
      `No completed turns recorded yet for "${label}" — no usage or evaluation data to derive a quality score from.`,
    );
  }
  return unknownDim(
    "usageHistoryStore.byModel[label].turns",
    `${totals.turns} completed turn(s) recorded for "${label}" (${totals.totalTokens} tokens total), but no eval/benchmark score is tracked for any model.`,
  );
}

export function scoreLocalModel(
  model: ModelInfo,
  usage: ModelUsageLookup,
  llamaStatus: string,
  isActive: boolean,
): TrustScorecard {
  const quality = usageEvidence(model.name, usage);

  const cost = dim("good", [
    { field: "ModelInfo.path", fact: "Runs locally via llama-server; no per-token API cost." },
  ]);

  const privacy = dim("good", [
    { field: "ModelInfo (local kind)", fact: "Local llama.cpp inference — prompts and responses never leave this device." },
  ]);

  const security = dim("good", [
    {
      field: "ModelInfo.is_external",
      fact: model.is_external
        ? "Registered via models_add_external from a user-chosen local .gguf file; the app never owns or auto-updates it."
        : "Downloaded through the app's own models_download command from its curated catalog.",
    },
  ]);

  let reliability: TrustDimensionScore;
  if (isActive) {
    reliability = dim(llamaStatus === "error" ? "poor" : llamaStatus === "ready" ? "good" : "unknown", [
      { field: "modelStore.llamaStatus", fact: `Current llama-server status: "${llamaStatus}".` },
    ]);
  } else {
    reliability = unknownDim("modelStore.llamaStatus", "Not the currently active model — no live status to report, and no historical uptime log is kept per model.");
  }

  const provenance = dim("good", [
    {
      field: "ModelInfo.is_external",
      fact: model.is_external
        ? "Added by the user as an external local model file (models_add_external)."
        : "Listed in the app's built-in curated model catalog (models_list_curated).",
    },
  ]);

  return {
    id: `model:local:${model.id}`,
    kind: "model",
    name: model.name,
    subtitle: model.repo,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

export function scoreOllamaModel(model: OllamaModelInfo, usage: ModelUsageLookup): TrustScorecard {
  const label = `Ollama · ${model.name}`;
  const quality = usageEvidence(label, usage);

  const cost = model.is_cloud
    ? dim("fair", [{ field: "OllamaModelInfo.is_cloud", fact: "is_cloud=true — this tag runs on Ollama's hosted cloud tier, which may carry cost beyond local hardware use." }])
    : dim("good", [{ field: "OllamaModelInfo.is_cloud", fact: "is_cloud=false — runs on local hardware; no metered API cost." }]);

  const privacy = model.is_cloud
    ? dim("fair", [{ field: "OllamaModelInfo.is_cloud", fact: "Requests for this tag are served by Ollama's cloud infrastructure, not this device." }])
    : dim("good", [{ field: "OllamaModelInfo.is_cloud", fact: "Runs on the local Ollama daemon; prompts stay on this device." }]);

  const security = unknownDim(
    "OllamaModelInfo",
    "No per-model security signal is tracked for Ollama tags beyond the daemon's own auth (see the Ollama panel's sign-in status).",
  );

  const reliability = unknownDim(
    "modelStore.ollamaModels",
    `Last modified/pulled at ${model.modified_at}; no historical error or uptime log is kept per Ollama tag.`,
  );

  const provenance = dim("good", [
    { field: "OllamaModelInfo.name", fact: "Pulled or imported into the local Ollama daemon by the user." },
  ]);

  return {
    id: `model:ollama:${model.name}`,
    kind: "model",
    name: label,
    subtitle: model.is_cloud ? "Ollama cloud" : "Ollama local",
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

export function scoreProviderModel(
  provider: ProviderConfig,
  modelId: string,
  usage: ModelUsageLookup,
  providerKeyError: Record<string, string>,
): TrustScorecard {
  const label = `${provider.id} · ${modelId}`;
  const quality = usageEvidence(label, usage);

  const cost = unknownDim(
    "ProviderConfig.base_url",
    `Cloud API model; Little Monkey doesn't track ${provider.label}'s per-token pricing — check the provider's own pricing page.`,
  );

  const privacy = dim("fair", [
    { field: "ProviderConfig.base_url", fact: `Requests go to ${provider.base_url}; prompt and response data leaves this device.` },
  ]);

  const security = provider.has_key
    ? dim("good", [{ field: "ProviderConfig.has_key", fact: "API key is stored in the OS keychain (providers_set_key), never in plain app config." }])
    : unknownDim("ProviderConfig.has_key", "No API key configured yet for this provider.");

  const keyError = providerKeyError[provider.id];
  const reliability = keyError
    ? dim("poor", [{ field: "modelStore.providerKeyError", fact: `Last key/model-list failure: "${keyError}"` }])
    : unknownDim("modelStore.providerKeyError", "No recorded failures this session; no historical uptime data is tracked for cloud providers.");

  const provenance = dim("good", [
    {
      field: "ProviderConfig.is_custom",
      fact: provider.is_custom
        ? `Custom OpenAI-compatible endpoint added by the user (${provider.base_url}).`
        : "Built-in preset provider shipped with the app.",
    },
  ]);

  return {
    id: `model:provider:${provider.id}:${modelId}`,
    kind: "model",
    name: `${provider.label} · ${modelId}`,
    subtitle: provider.base_url,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

// ---------------------------------------------------------------------------
// Connectors
// ---------------------------------------------------------------------------

export function scoreConnector(account: ConnectorAccount): TrustScorecard {
  const quality = unknownDim("ConnectorAccount", "No usage or evaluation data is tracked for connector accounts.");

  const cost = unknownDim(
    "ConnectorAccount.provider",
    `Cost depends on ${account.provider}'s own plan; Little Monkey doesn't track connector pricing.`,
  );

  const scopeList = account.scopes.length ? account.scopes.join(", ") : "none declared";
  const privacy = dim("fair", [
    { field: "ConnectorAccount.provider", fact: `${account.provider} is a cloud-hosted third-party service.` },
    { field: "ConnectorAccount.scopes", fact: `Granted scopes: ${scopeList}.` },
  ]);

  let security: TrustDimensionScore;
  if (account.credential_ref) {
    security = dim("good", [
      { field: "ConnectorAccount.credential_ref", fact: `Secret stored in the OS keychain under reference "${account.credential_ref}", never in plain config.` },
    ]);
  } else if (account.provider === "github") {
    security = dim("good", [
      { field: "ConnectorAccount.credential_ref", fact: "No stored credential — identity comes from the system's own `gh` CLI auth, not a token this app holds." },
    ]);
  } else {
    security = unknownDim("ConnectorAccount.credential_ref", "No credential reference on file.");
  }

  let reliability: TrustDimensionScore;
  if (account.last_error) {
    reliability = dim("poor", [{ field: "ConnectorAccount.last_error", fact: `Last verification failed: "${account.last_error}"` }]);
  } else if (account.last_verified_at) {
    reliability = dim("good", [
      { field: "ConnectorAccount.last_verified_at", fact: `Last verified successfully at ${new Date(account.last_verified_at).toLocaleString()}.` },
    ]);
  } else {
    reliability = unknownDim("ConnectorAccount.last_verified_at", "Never successfully verified yet.");
  }

  const provenance = dim("good", [
    { field: "ConnectorAccount.created_at", fact: `Added by the user via Settings > Connectors on ${new Date(account.created_at).toLocaleDateString()}.` },
  ]);

  return {
    id: `connector:${account.id}`,
    kind: "connector",
    name: account.label,
    subtitle: account.provider,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

// ---------------------------------------------------------------------------
// MCP servers
// ---------------------------------------------------------------------------

export function scoreMcpServer(server: McpServerInfo): TrustScorecard {
  const quality = unknownDim(
    "McpServerInfo.tools",
    `${server.tools.length} tool(s) currently cached from live introspection, but no eval/success-rate data is tracked.`,
  );

  const cost = unknownDim("McpServerInfo", "No pricing data is tracked for MCP servers — cost (if any) depends on the server itself.");

  const privacy =
    server.transport.type === "http"
      ? dim("fair", [{ field: "McpServerInfo.transport", fact: `Remote HTTP server at ${server.transport.url}; requests leave this device.` }])
      : dim("good", [{ field: "McpServerInfo.transport", fact: `Runs as a local child process ("${server.transport.command}"); no network hop is inherent to the transport itself.` }]);

  const securityEvidence: TrustEvidenceItem[] = [];
  let securityLevel: TrustLevel;
  if (server.transport.type === "stdio") {
    securityEvidence.push({
      field: "McpServerInfo.transport.command",
      fact: `Runs as a local child process from command "${server.transport.command}" — the app trusts whatever binary that command resolves to.`,
    });
    securityLevel = "fair";
  } else {
    securityEvidence.push({
      field: "McpServerInfo.hasHttpToken",
      fact: server.hasHttpToken
        ? "Bearer token stored in the OS keychain (mcp_set_http_token), never in plain config."
        : "No bearer token saved for this HTTP server.",
    });
    securityLevel = server.hasHttpToken ? "good" : "unknown";
  }
  if (server.toolAllowlist) {
    securityEvidence.push({
      field: "McpServerInfo.toolAllowlist",
      fact: `Restricted to an explicit tool allowlist: ${server.toolAllowlist.join(", ")}.`,
    });
    if (securityLevel === "fair" || securityLevel === "unknown") securityLevel = "good";
  } else {
    securityEvidence.push({ field: "McpServerInfo.toolAllowlist", fact: "No tool allowlist set — every tool this server exposes is available." });
  }
  const security = dim(securityLevel, securityEvidence);

  let reliability: TrustDimensionScore;
  if (server.status === "connected") {
    reliability = dim("good", [{ field: "McpServerInfo.status", fact: "Currently connected." }]);
  } else if (server.status === "error") {
    reliability = dim("poor", [{ field: "McpServerInfo.error", fact: `Last connection error: "${server.error ?? "unknown error"}"` }]);
  } else {
    reliability = unknownDim("McpServerInfo.status", `Current status: "${server.status}" — no historical uptime log is tracked.`);
  }

  const provenance = dim("good", [
    { field: "McpServerInfo.id", fact: "Configured by the user in mcp_servers.json — no MCP server ships built into the app." },
  ]);

  return {
    id: `mcp_server:${server.id}`,
    kind: "mcp_server",
    name: server.label,
    subtitle: server.transport.type === "http" ? server.transport.url : server.transport.command,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

// ---------------------------------------------------------------------------
// Native skills
// ---------------------------------------------------------------------------

export function scoreSkill(skill: NativeSkillDescriptor): TrustScorecard {
  const quality = unknownDim(
    "NativeSkillDescriptor",
    `${skill.file_count} file(s), ${skill.total_bytes} byte(s) — no eval/success-rate data is tracked for skills.`,
  );

  const cost = unknownDim(
    "NativeSkillDescriptor",
    "Skills have no direct pricing; cost depends on which tools or models the skill's own instructions invoke at run time.",
  );

  const privacy = unknownDim(
    "NativeSkillDescriptor.instructions",
    "A skill's SKILL.md instructions may direct the agent to call network-reaching tools; no static data-flow analysis exists to say whether this one does.",
  );

  const securityEvidence: TrustEvidenceItem[] = [];
  let securityLevel: TrustLevel;
  if (skill.allowed_tools.length > 0) {
    securityEvidence.push({
      field: "NativeSkillDescriptor.allowed_tools",
      fact: `Restricted to ${skill.allowed_tools.length} tool(s) while active: ${skill.allowed_tools.join(", ")}.`,
    });
    securityLevel = "good";
  } else {
    securityEvidence.push({
      field: "NativeSkillDescriptor.allowed_tools",
      fact: "allowed_tools is empty — this skill can invoke any tool available to the current profile.",
    });
    securityLevel = "fair";
  }
  if (skill.permissions.length > 0) {
    securityEvidence.push({ field: "NativeSkillDescriptor.permissions", fact: `Declares ${skill.permissions.length} permission(s): ${skill.permissions.join(", ")}.` });
  }
  const security = dim(securityLevel, securityEvidence);

  const reliability = skill.eligibility.eligible
    ? dim("good", [{ field: "NativeSkillDescriptor.eligibility.eligible", fact: "All eligibility checks (OS, required binaries/env) pass on this machine." }])
    : dim("poor", [
        {
          field: "NativeSkillDescriptor.eligibility",
          fact: [
            skill.eligibility.unsupported_os ? `unsupported OS (${skill.eligibility.current_os})` : null,
            skill.eligibility.missing_bins.length ? `missing binaries: ${skill.eligibility.missing_bins.join(", ")}` : null,
            skill.eligibility.missing_env.length ? `missing env vars: ${skill.eligibility.missing_env.join(", ")}` : null,
          ]
            .filter(Boolean)
            .join("; ") || "not eligible on this machine",
        },
      ]);

  let provenance: TrustDimensionScore;
  if (skill.source.kind === "signed_package") {
    provenance = dim("good", [
      { field: "NativeSkillDescriptor.source", fact: `Installed from a signed package ("${skill.source.package_id}"), sha256 ${skill.sha256}.` },
    ]);
  } else if (skill.git_repository) {
    provenance = dim("fair", [
      { field: "NativeSkillDescriptor.git_repository", fact: `Installed from git repository ${skill.git_repository} (${skill.source.kind} scope).` },
    ]);
  } else {
    provenance = dim("good", [
      { field: "NativeSkillDescriptor.source", fact: `Installed locally by the user (${skill.source.kind} scope, ${skill.source.path}).` },
    ]);
  }

  return {
    id: `skill:${skill.command}`,
    kind: "skill",
    name: skill.name,
    subtitle: `/${skill.command}`,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

// ---------------------------------------------------------------------------
// Workflows (user-authored, from ecosystemStore.workflows / recipes_list)
// ---------------------------------------------------------------------------

function runStats(workflowId: string, histories: WorkflowRunHistory[]) {
  const runs = histories.filter((h) => h.workflow_id === workflowId && h.status !== "running");
  const succeeded = runs.filter((h) => h.status === "succeeded").length;
  return { total: runs.length, succeeded };
}

export function scoreWorkflow(
  workflow: WorkflowDefinition,
  histories: WorkflowRunHistory[],
  catalog: PackageCatalogEntry[],
): TrustScorecard {
  const { total, succeeded } = runStats(workflow.workflow_id, histories);
  let quality: TrustDimensionScore;
  if (total === 0) {
    quality = unknownDim("ecosystemStore.histories", "No recorded runs yet for this workflow (workflows_histories).");
  } else {
    const rate = succeeded / total;
    quality = dim(rate >= 0.8 ? "good" : rate >= 0.5 ? "fair" : "poor", [
      { field: "WorkflowRunHistory.status", fact: `${succeeded} of ${total} recorded run(s) succeeded.` },
    ]);
  }

  const cost = dim(workflow.budgets.maximum_model_calls === 0 ? "good" : "fair", [
    {
      field: "WorkflowDefinition.budgets",
      fact: `Declared ceiling: ${workflow.budgets.maximum_model_calls} model call(s), ${workflow.budgets.maximum_cost_microunits} cost-unit(s) per run (a maximum, not actual spend).`,
    },
  ]);

  const externalNodeKinds = workflow.nodes.filter((n) => n.kind.kind === "mcp" || n.kind.kind === "pull_request" || n.kind.kind === "shell");
  const privacy =
    externalNodeKinds.length > 0
      ? dim("fair", [
          { field: "WorkflowDefinition.nodes[].kind", fact: `${externalNodeKinds.length} node(s) reach outside this device: ${externalNodeKinds.map((n) => n.node_id).join(", ")}.` },
        ])
      : dim("good", [{ field: "WorkflowDefinition.nodes[].kind", fact: "No mcp/pull_request/shell node reaches outside this device." }]);

  const secretNodes = workflow.nodes.filter((n) => n.secret_ids.length > 0);
  const hasApprovalGate = workflow.nodes.some((n) => n.kind.kind === "human_approval");
  let securityLevel: TrustLevel = "good";
  const securityEvidence: TrustEvidenceItem[] = [];
  if (secretNodes.length > 0) {
    securityEvidence.push({ field: "WorkflowNode.secret_ids", fact: `${secretNodes.length} node(s) reference secrets: ${secretNodes.map((n) => n.node_id).join(", ")}.` });
    securityLevel = hasApprovalGate ? "good" : "fair";
  } else {
    securityEvidence.push({ field: "WorkflowNode.secret_ids", fact: "No node in this workflow references a secret." });
  }
  securityEvidence.push({
    field: "WorkflowNode.kind (human_approval)",
    fact: hasApprovalGate ? "Includes a human_approval node — at least one gate requires explicit user confirmation." : "No human_approval node in this workflow.",
  });
  const security = dim(securityLevel, securityEvidence);

  const { total: reliabilityTotal, succeeded: reliabilitySucceeded } = runStats(workflow.workflow_id, histories);
  const reliability =
    reliabilityTotal === 0
      ? unknownDim("ecosystemStore.histories", "No recorded runs yet — no failure/reconciliation rate to report.")
      : dim(reliabilitySucceeded === reliabilityTotal ? "good" : reliabilitySucceeded / reliabilityTotal >= 0.5 ? "fair" : "poor", [
          { field: "WorkflowRunHistory.status", fact: `${reliabilityTotal - reliabilitySucceeded} of ${reliabilityTotal} recorded run(s) did not succeed (failed/cancelled/needs_reconciliation).` },
        ]);

  const catalogMatch = catalog.find((entry) => entry.manifest.display_name === workflow.name);
  const provenance = catalogMatch
    ? dim(catalogMatch.trust?.signed ? "good" : "fair", [
        { field: "PackageManifest.provenance.publisher", fact: `Matches a catalog package published by "${catalogMatch.manifest.provenance.publisher}".` },
        { field: "TrustEvidence.signed", fact: catalogMatch.trust?.signed ? "Package bundle is signed." : "Package bundle is not signed." },
      ])
    : dim("good", [{ field: "ecosystemStore.workflows", fact: "Authored locally in the workflow editor (recipes_list) — no matching marketplace catalog entry." }]);

  return {
    id: `workflow:${workflow.workflow_id}`,
    kind: "workflow",
    name: workflow.name,
    subtitle: `v${workflow.workflow_version}`,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}

// ---------------------------------------------------------------------------
// Plugins (installed marketplace packages — ecosystemStore.plugins)
// ---------------------------------------------------------------------------

const HIGH_RISK_PERMISSIONS = new Set(["execute_process", "install_executable", "read_raw_keychain"]);

export function scorePlugin(plugin: PluginRuntimeDescriptor, catalog: PackageCatalogEntry[]): TrustScorecard {
  const quality = unknownDim(
    "PluginRuntimeDescriptor.components",
    `${plugin.components.length} component(s) bundled — no eval/success-rate data is tracked for installed packages.`,
  );

  const usesModel = plugin.permissions.some((p) => p.kind === "use_model");
  const cost = unknownDim(
    "PluginRuntimeDescriptor.permissions",
    usesModel
      ? "Declares a use_model permission — actual cost depends on which model it calls; no pricing is tracked here."
      : "No pricing data is tracked for installed packages.",
  );

  const networkPerms = plugin.permissions.filter((p) => p.kind === "network" || p.kind === "invoke_mcp_tool");
  const privacy =
    networkPerms.length > 0
      ? dim("fair", [{ field: "PackagePermission.kind", fact: `Declares network-reaching permission(s): ${networkPerms.map((p) => `${p.kind} (${p.reason})`).join("; ")}.` }])
      : dim("good", [{ field: "PackagePermission.kind", fact: "No network or invoke_mcp_tool permission declared." }]);

  const highRisk = plugin.permissions.filter((p) => HIGH_RISK_PERMISSIONS.has(p.kind));
  const securityEvidence: TrustEvidenceItem[] = [
    { field: "PluginRuntimeDescriptor.signed", fact: plugin.signed ? "Bundle is signed and verified against a trust root." : "Bundle is NOT signed." },
  ];
  if (highRisk.length > 0) {
    securityEvidence.push({ field: "PackagePermission.kind", fact: `Declares high-risk permission(s): ${highRisk.map((p) => `${p.kind} (${p.reason})`).join("; ")}.` });
  }
  const securityLevel: TrustLevel = !plugin.signed ? "poor" : highRisk.length > 0 ? "fair" : "good";
  const security = dim(securityLevel, securityEvidence);

  const issueNote = plugin.issues.length > 0 ? ` Issues: ${plugin.issues.join("; ")}.` : "";
  let reliability: TrustDimensionScore;
  if (plugin.health === "healthy") {
    reliability = dim("good", [{ field: "PluginRuntimeDescriptor.health", fact: `health="healthy".${issueNote}` }]);
  } else if (plugin.health === "blocked" || plugin.health === "corrupt") {
    reliability = dim("poor", [{ field: "PluginRuntimeDescriptor.health", fact: `health="${plugin.health}".${issueNote}` }]);
  } else {
    reliability = dim("fair", [{ field: "PluginRuntimeDescriptor.health", fact: `health="${plugin.health}".${issueNote}` }]);
  }

  const catalogMatch = catalog.find((entry) => entry.manifest.package_id === plugin.package_id);
  const provenance = catalogMatch
    ? dim(catalogMatch.trust?.signed && catalogMatch.trust.trust_root_id ? "good" : "fair", [
        { field: "PackageManifest.provenance.publisher", fact: `Published by "${catalogMatch.manifest.provenance.publisher}", source revision ${catalogMatch.manifest.provenance.source_revision}.` },
        {
          field: "TrustEvidence",
          fact: catalogMatch.trust
            ? `signed=${catalogMatch.trust.signed}, trust_root_id=${catalogMatch.trust.trust_root_id ?? "none"}.`
            : "no trust evidence recorded in the catalog entry.",
        },
      ])
    : unknownDim("ecosystemStore.catalog", "Installed package has no matching catalog entry (e.g. imported via a portable export) — publisher/signing evidence unavailable.");

  return {
    id: `plugin:${plugin.package_id}`,
    kind: "plugin",
    name: plugin.name,
    subtitle: plugin.version ?? plugin.kind,
    dimensions: { quality, cost, privacy, security, reliability, provenance },
  };
}
