import { useEffect, useState } from "react";
import { AlertTriangle, Check, ChevronRight, CircleStop, Code2, GitBranch, History, Import, ListRestart, Play, Plus, RefreshCw, Save, ServerCog, Trash2, Workflow } from "lucide-react";
import { Button, StatusPill } from "../ui";
import {
  ecosystemClient,
  type EffectClass,
  type InputBinding,
  type LegacyRecipeV1,
  type WorkflowDefinition,
  type WorkflowHumanApprovalChallenge,
  type WorkflowNode,
  type WorkflowNodeKind,
  type WorkflowRunHistory,
  type WorkflowRunRequest,
  type WorkflowTrigger,
} from "../../lib/ecosystemClient";
import { useT } from "../../lib/i18n";
import { makeWorkflowNode, newWorkflowDefinition, useEcosystemStore } from "../../store/ecosystemStore";
import { useMcpStore } from "../../store/mcpStore";

const FIELD = "h-9 w-full rounded-lg border border-border bg-surface-2 px-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent";
const AREA = "w-full rounded-lg border border-border bg-surface-2 px-3 py-2 font-mono text-xs text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent";

type AddableNodeKind = Exclude<WorkflowNodeKind["kind"], "legacy_recipe">;

const NODE_KINDS: AddableNodeKind[] = [
  "prompt_model", "agent", "subagent", "tool", "mcp", "browser", "git", "pull_request", "shell", "verify", "transform",
  "condition", "bounded_loop", "human_approval", "artifact", "output",
];

const ADAPTER_ACTIONS = {
  browser: [
    ["start", "local_mutation"], ["list", "read_only"], ["navigate", "read_only"],
    ["inspect", "read_only"], ["click", "external_mutation"], ["type_text", "external_mutation"],
    ["scroll", "read_only"], ["screenshot", "read_only"], ["capture_evidence", "read_only"],
    ["stop", "local_mutation"],
  ],
  git: [
    ["list_worktrees", "read_only"], ["inspect_worktree", "read_only"], ["prepare_mutation", "read_only"],
    ["execute_local_mutation", "local_mutation"], ["execute_push", "external_mutation"],
  ],
  pull_request: [
    ["auth_status", "read_only"], ["read_issue", "read_only"], ["read_pull_request", "read_only"],
    ["read_review_threads", "read_only"], ["read_checks", "read_only"], ["review_pull_request", "read_only"],
    ["review_reports", "read_only"], ["prepare_mutation", "read_only"],
    ["execute_external_mutation", "external_mutation"], ["execute_patch_task", "local_mutation"],
  ],
} as const satisfies Partial<Record<AddableNodeKind, readonly (readonly [string, EffectClass])[]>>;

function adapterArguments(kind: "browser" | "git" | "pull_request", action: string): unknown {
  if (kind === "browser") {
    if (action === "start") return { url: "http://127.0.0.1:3000", grant: { allowedOrigins: ["http://127.0.0.1:3000"], allowLoopback: true }, limits: { timeoutMs: 60_000, maxActions: 100, maxDomBytes: 4_194_304, maxScreenshotBytes: 12_582_912, maxLogEntries: 2_000 } };
    if (action === "list") return {};
    if (action === "navigate") return { sessionId: "", url: "https://example.com" };
    if (action === "click") return { sessionId: "", selector: "button" };
    if (action === "type_text") return { sessionId: "", selector: "input", text: "" };
    if (action === "scroll") return { sessionId: "", x: 0, y: 600 };
    return { sessionId: "" };
  }
  if (kind === "git") {
    if (action === "list_worktrees") return {};
    if (action === "inspect_worktree") return { worktreeId: "" };
    if (action === "prepare_mutation") return { mutation: { kind: "stage", payload: { worktreeId: "", paths: [] } } };
    return { mutation: { kind: action === "execute_push" ? "push" : "stage", payload: action === "execute_push" ? { worktreeId: "", remote: "origin" } : { worktreeId: "", paths: [] } }, digest: "", confirmation: "" };
  }
  if (action === "auth_status") return {};
  if (action === "prepare_mutation") return { mutation: { kind: "create_draft_pr", payload: { worktreeId: "", base: "main", title: "", body: "" } } };
  if (action === "execute_external_mutation") return { mutation: { kind: "create_draft_pr", payload: { worktreeId: "", base: "main", title: "", body: "" } }, digest: "", confirmation: "" };
  if (action === "execute_patch_task") return { mutation: { kind: "queue_patch_task", payload: { worktreeId: "", prNumber: 1, commentId: 1, model: "" } }, digest: "", confirmation: "" };
  if (action === "review_pull_request") return { worktreeId: "", number: 1, model: "" };
  return { worktreeId: "", number: 1 };
}

function clone<T>(value: T): T {
  return structuredClone(value);
}

function newRunId(prefix = "run"): string {
  return `${prefix}-${crypto.randomUUID()}`;
}

function statusTone(status: WorkflowRunHistory["status"]): "neutral" | "success" | "warning" | "danger" {
  if (status === "succeeded") return "success";
  if (status === "running") return "warning";
  if (status === "needs_reconciliation") return "warning";
  return "danger";
}

interface PositionedNode {
  node: WorkflowNode;
  level: number;
  x: number;
  y: number;
}

function dependencyIds(node: WorkflowNode): string[] {
  return Object.values(node.inputs).flatMap((binding) => binding.source === "node_output" ? [binding.node_id] : []);
}

function layoutNodes(nodes: WorkflowNode[]): PositionedNode[] {
  const byId = new Map(nodes.map((node) => [node.node_id, node]));
  const memo = new Map<string, number>();
  function levelFor(nodeId: string, visiting = new Set<string>()): number {
    if (memo.has(nodeId)) return memo.get(nodeId) ?? 0;
    if (visiting.has(nodeId)) return 0;
    visiting.add(nodeId);
    const node = byId.get(nodeId);
    const dependencies = node ? dependencyIds(node).filter((id) => byId.has(id)) : [];
    const level = dependencies.length === 0 ? 0 : Math.max(...dependencies.map((id) => levelFor(id, visiting))) + 1;
    visiting.delete(nodeId);
    memo.set(nodeId, level);
    return level;
  }
  const rows = new Map<number, number>();
  return nodes.map((node) => {
    const level = levelFor(node.node_id);
    const row = rows.get(level) ?? 0;
    rows.set(level, row + 1);
    return { node, level, x: 24 + level * 205, y: 24 + row * 92 };
  });
}

function WorkflowDag({ definition, selectedNodeId, onSelect }: { definition: WorkflowDefinition; selectedNodeId: string | null; onSelect: (nodeId: string) => void }) {
  const positioned = layoutNodes(definition.nodes);
  const byId = new Map(positioned.map((item) => [item.node.node_id, item]));
  const width = Math.max(700, ...positioned.map((item) => item.x + 180));
  const height = Math.max(180, ...positioned.map((item) => item.y + 70));
  return (
    <div className="overflow-auto rounded-xl border border-border bg-surface [overscroll-behavior:contain]">
      <svg role="img" aria-label="Workflow dependency graph" width={width} height={height} className="block">
        <defs><marker id="workflow-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5" markerHeight="5" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="var(--c-faint)" /></marker></defs>
        {positioned.flatMap((target) => dependencyIds(target.node).map((dependency) => {
          const source = byId.get(dependency);
          if (!source) return null;
          const x1 = source.x + 164;
          const y1 = source.y + 29;
          const x2 = target.x;
          const y2 = target.y + 29;
          return <path key={`${source.node.node_id}:${target.node.node_id}`} d={`M ${x1} ${y1} C ${x1 + 38} ${y1}, ${x2 - 38} ${y2}, ${x2} ${y2}`} fill="none" stroke="var(--c-faint)" strokeWidth="1.5" markerEnd="url(#workflow-arrow)" />;
        }))}
        {positioned.map(({ node, x, y }) => (
          <g key={node.node_id} role="button" tabIndex={0} aria-label={`${node.node_id}, ${node.kind.kind}`} onClick={() => onSelect(node.node_id)} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") onSelect(node.node_id); }} className="cursor-pointer focus:outline-none">
            <rect x={x} y={y} width="164" height="58" rx="9" fill={selectedNodeId === node.node_id ? "var(--c-accent-soft)" : "var(--c-surface-2)"} stroke={selectedNodeId === node.node_id ? "var(--c-accent)" : "var(--c-border)"} strokeWidth={selectedNodeId === node.node_id ? 2 : 1} />
            <text x={x + 12} y={y + 23} fill="var(--c-foreground)" fontSize="12" fontWeight="600">{node.node_id.length > 21 ? `${node.node_id.slice(0, 20)}…` : node.node_id}</text>
            <text x={x + 12} y={y + 43} fill="var(--c-muted)" fontSize="11">{node.kind.kind}</text>
          </g>
        ))}
      </svg>
    </div>
  );
}

function approvalSummary(node: WorkflowNode): string {
  const binding = node.inputs.summary;
  if (binding?.source === "literal" && binding.value.kind === "string") return binding.value.value;
  return `Approve workflow node ${node.node_id}`;
}

export function EcosystemWorkflowDesigner() {
  const { t } = useT();
  const {
    workflows,
    workflowIr,
    activeRunId,
    busy,
    refreshWorkflows,
    validateWorkflow,
    saveWorkflow,
    deleteWorkflow,
    runWorkflow,
    cancelWorkflow,
  } = useEcosystemStore();
  const { servers: mcpServers, refresh: refreshMcpServers } = useMcpStore();
  const [definition, setDefinition] = useState<WorkflowDefinition>(() => newWorkflowDefinition());
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(definition.nodes[0]?.node_id ?? null);
  const [nodeIdText, setNodeIdText] = useState("");
  const [nodeKindText, setNodeKindText] = useState("");
  const [nodeInputsText, setNodeInputsText] = useState("");
  const [nodeAdvancedText, setNodeAdvancedText] = useState("");
  const [definitionText, setDefinitionText] = useState("");
  const [addKind, setAddKind] = useState<AddableNodeKind>("transform");
  const [edge, setEdge] = useState({ sourceNode: "", sourcePort: "out", targetNode: "", targetPort: "input" });
  const [runId, setRunId] = useState(() => newRunId());
  const [runInputsText, setRunInputsText] = useState("{}");
  const [secretBindingsText, setSecretBindingsText] = useState("{}");
  const [runTriggerText, setRunTriggerText] = useState('{"kind":"manual"}');
  const [approvalChallenges, setApprovalChallenges] = useState<Record<string, WorkflowHumanApprovalChallenge>>({});
  const [approvedNodes, setApprovedNodes] = useState<Set<string>>(new Set());
  const [legacyText, setLegacyText] = useState(JSON.stringify({ version: 1, name: "imported-recipe", target: { provider: null, model: null, ollama: "qwen2.5:7b", local_url: null }, permission_mode: "ask", system: null, prompt: "{{prompt}}", params: { prompt: null }, maximum_iterations: 1, timeout_seconds: 60 } satisfies LegacyRecipeV1, null, 2));
  const [showLegacy, setShowLegacy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const [triggerIds, setTriggerIds] = useState<string[]>([]);
  const [triggerStatus, setTriggerStatus] = useState<string | null>(null);
  const selectedNode = definition.nodes.find((node) => node.node_id === selectedNodeId) ?? null;
  const selectedAdapterActions = selectedNode && selectedNode.kind.kind in ADAPTER_ACTIONS
    ? ADAPTER_ACTIONS[selectedNode.kind.kind as keyof typeof ADAPTER_ACTIONS]
    : null;
  const selectedMcpKind = selectedNode?.kind.kind === "mcp" ? selectedNode.kind : null;
  const exists = workflows.some((workflow) => workflow.workflow_id === definition.workflow_id);

  useEffect(() => {
    void (async () => {
      try {
        await refreshMcpServers();
        await ecosystemClient.refreshWorkflowCapabilities();
      } catch (error) {
        setLocalError(error instanceof Error ? error.message : String(error));
      }
    })();
  }, [refreshMcpServers]);

  useEffect(() => {
    if (!selectedNode) {
      setNodeIdText(""); setNodeKindText(""); setNodeInputsText(""); setNodeAdvancedText("");
      return;
    }
    setNodeIdText(selectedNode.node_id);
    setNodeKindText(JSON.stringify(selectedNode.kind, null, 2));
    setNodeInputsText(JSON.stringify(selectedNode.inputs, null, 2));
    const { node_id: _id, kind: _kind, inputs: _inputs, ...advanced } = selectedNode;
    setNodeAdvancedText(JSON.stringify(advanced, null, 2));
  }, [selectedNode]);

  useEffect(() => {
    const { nodes: _nodes, workflow_id: _id, workflow_version: _version, name: _name, ...advanced } = definition;
    setDefinitionText(JSON.stringify(advanced, null, 2));
  }, [definition.workflow_id, definition.inputs, definition.secrets, definition.outputs, definition.budgets, definition.maximum_concurrency, definition.triggers]);

  function loadDefinition(next: WorkflowDefinition) {
    const copied = clone(next);
    setDefinition(copied);
    setSelectedNodeId(copied.nodes[0]?.node_id ?? null);
    const { nodes: _nodes, workflow_id: _id, workflow_version: _version, name: _name, ...advanced } = copied;
    setDefinitionText(JSON.stringify(advanced, null, 2));
    setLocalError(null);
    setConfirmDelete(false);
    setTriggerIds([]);
    setTriggerStatus(null);
    setWorkflowRunDefaults(copied);
  }

  function setWorkflowRunDefaults(next: WorkflowDefinition) {
    const inputs = Object.fromEntries(Object.entries(next.inputs).map(([id, type]) => [id, type.kind === "string" ? { kind: "string", value: "" } : type.kind === "boolean" ? { kind: "boolean", value: false } : type.kind === "integer" ? { kind: "integer", value: 0 } : type.kind === "decimal" ? { kind: "decimal", value: 0 } : { kind: "json", value: null }]));
    setRunInputsText(JSON.stringify(inputs, null, 2));
    setSecretBindingsText(JSON.stringify(Object.fromEntries(Object.keys(next.secrets).map((id) => [id, { secret_id: id, vault_reference: `keychain://${id}` }])), null, 2));
    setRunTriggerText(JSON.stringify(next.triggers.find((trigger) => trigger.kind === "manual") ?? next.triggers[0] ?? { kind: "manual" }, null, 2));
    setRunId(newRunId());
    setApprovalChallenges({});
    setApprovedNodes(new Set());
  }

  function buildDraft(): WorkflowDefinition {
    const base = clone(definition);
    const advancedDefinition = JSON.parse(definitionText) as Omit<WorkflowDefinition, "workflow_id" | "workflow_version" | "name" | "nodes">;
    const next: WorkflowDefinition = {
      ...base,
      ...advancedDefinition,
      workflow_id: base.workflow_id,
      workflow_version: base.workflow_version,
      name: base.name,
      nodes: base.nodes,
    };
    if (selectedNodeId) {
      const index = next.nodes.findIndex((node) => node.node_id === selectedNodeId);
      if (index >= 0) {
        const nextNodeId = nodeIdText.trim();
        if (!nextNodeId) throw new Error("Node ID cannot be empty.");
        if (nextNodeId !== selectedNodeId && next.nodes.some((node) => node.node_id === nextNodeId)) {
          throw new Error(`Node ID ${nextNodeId} already exists.`);
        }
        const kind = JSON.parse(nodeKindText) as WorkflowNodeKind;
        const inputs = JSON.parse(nodeInputsText) as Record<string, InputBinding>;
        const advanced = JSON.parse(nodeAdvancedText) as Omit<WorkflowNode, "node_id" | "kind" | "inputs">;
        if (nextNodeId !== selectedNodeId) {
          next.nodes = next.nodes.map((node) => ({
            ...node,
            inputs: Object.fromEntries(Object.entries(node.inputs).map(([port, binding]) => [port, binding.source === "node_output" && binding.node_id === selectedNodeId ? { ...binding, node_id: nextNodeId } : binding])),
            permission_policy: { ...node.permission_policy, approval_node_id: node.permission_policy.approval_node_id === selectedNodeId ? nextNodeId : node.permission_policy.approval_node_id },
            guard: node.guard?.condition_node_id === selectedNodeId ? { ...node.guard, condition_node_id: nextNodeId } : node.guard,
          }));
          next.outputs = Object.fromEntries(Object.entries(next.outputs).map(([id, output]) => [id, output.binding.source === "node_output" && output.binding.node_id === selectedNodeId ? { ...output, binding: { ...output.binding, node_id: nextNodeId } } : output]));
          next.secrets = Object.fromEntries(Object.entries(next.secrets).map(([id, secret]) => [id, { ...secret, allowed_node_ids: secret.allowed_node_ids.map((nodeId) => nodeId === selectedNodeId ? nextNodeId : nodeId) }]));
        }
        next.nodes[index] = { ...advanced, node_id: nextNodeId, kind, inputs };
      }
    }
    return next;
  }

  function applyDraft(): WorkflowDefinition | null {
    try {
      const next = buildDraft();
      setDefinition(next);
      if (selectedNodeId && nodeIdText.trim() && nodeIdText.trim() !== selectedNodeId) setSelectedNodeId(nodeIdText.trim());
      setLocalError(null);
      return next;
    } catch (error) {
      setLocalError(error instanceof Error ? error.message : String(error));
      return null;
    }
  }

  function setAgentProfile(profile: string) {
    if (!selectedNode || (selectedNode.kind.kind !== "agent" && selectedNode.kind.kind !== "subagent")) return;
    const kind = selectedNode.kind.kind;
    setDefinition((current) => ({
      ...current,
      nodes: current.nodes.map((node) => node.node_id === selectedNode.node_id
        ? { ...node, kind: { kind, agent_profile: profile, effect: "read_only" } }
        : node),
    }));
  }

  function setMcpTarget(serverId: string, toolName?: string) {
    if (!selectedNode || selectedNode.kind.kind !== "mcp") return;
    const server = mcpServers.find((candidate) => candidate.id === serverId);
    const allowedTools = server?.toolAllowlist ?? [];
    const nextTool = toolName ?? (allowedTools.includes(selectedNode.kind.tool_name) ? selectedNode.kind.tool_name : allowedTools[0] ?? "tool");
    setDefinition((current) => ({
      ...current,
      nodes: current.nodes.map((node) => node.node_id === selectedNode.node_id
        ? { ...node, kind: { kind: "mcp", server_id: serverId, tool_name: nextTool, effect: "external_mutation" } }
        : node),
    }));
  }

  function setAdapterAction(action: string) {
    if (!selectedNode || !selectedAdapterActions) return;
    const effect = selectedAdapterActions.find(([candidate]) => candidate === action)?.[1];
    if (!effect) return;
    const kind = selectedNode.kind.kind;
    if (kind !== "browser" && kind !== "git" && kind !== "pull_request") return;
    const mutation = effect === "local_mutation" || effect === "external_mutation";
    setDefinition((current) => {
      let approvalNodeId = selectedNode.permission_policy.approval_node_id;
      const additions: WorkflowNode[] = [];
      if (mutation && !approvalNodeId) {
        const approval = makeWorkflowNode("human_approval", current.nodes.length + 1);
        while (current.nodes.some((node) => node.node_id === approval.node_id)) approval.node_id = `${approval.node_id}-next`;
        const summary = approval.inputs.summary;
        if (summary?.source === "literal" && summary.value.kind === "string") {
          summary.value.value = `Approve ${kind}.${action} for ${selectedNode.node_id}`;
        }
        approvalNodeId = approval.node_id;
        additions.push(approval);
      }
      const permission = kind === "browser" ? "browser_control" : kind === "git" ? "git_write" : "github_write";
      const argumentsBinding: InputBinding = { source: "literal", value: { kind: "json", value: adapterArguments(kind, action) } };
      const nodes = current.nodes.map((node) => node.node_id !== selectedNode.node_id ? node : {
        ...node,
        kind: { kind, action, effect } as WorkflowNodeKind,
        inputs: mutation
          ? { ...node.inputs, arguments: argumentsBinding, approval: { source: "node_output", node_id: approvalNodeId!, port: "out" } as InputBinding }
          : { ...Object.fromEntries(Object.entries(node.inputs).filter(([port]) => port !== "approval")), arguments: argumentsBinding },
        permission_policy: mutation
          ? { permission_ids: [permission], approval_node_id: approvalNodeId }
          : { permission_ids: [], approval_node_id: null },
        idempotency: mutation ? { kind: "keyed", key_template: `${kind}:${action}:${node.node_id}:{run_id}` } : { kind: "none" },
        replay: mutation ? "requires_approval" as const : "safe" as const,
      });
      return { ...current, nodes: [...nodes, ...additions] };
    });
  }

  function addNode() {
    const next = makeWorkflowNode(addKind, definition.nodes.length + 1);
    while (definition.nodes.some((node) => node.node_id === next.node_id)) next.node_id = `${next.node_id}-next`;
    if (addKind === "shell" || addKind === "mcp") {
      const approval = makeWorkflowNode("human_approval", definition.nodes.length + 2);
      while (definition.nodes.some((node) => node.node_id === approval.node_id) || approval.node_id === next.node_id) approval.node_id = `${approval.node_id}-next`;
      next.inputs.approval = { source: "node_output", node_id: approval.node_id, port: "out" };
      next.permission_policy = { permission_ids: [addKind === "shell" ? "execute_process" : "mcp_tool_call"], approval_node_id: approval.node_id };
      next.idempotency = { kind: "keyed", key_template: `${addKind}:${next.node_id}:{run_id}` };
      next.replay = "requires_approval";
      setDefinition((current) => ({ ...current, nodes: [...current.nodes, approval, next] }));
    } else {
      setDefinition((current) => ({ ...current, nodes: [...current.nodes, next] }));
    }
    setSelectedNodeId(next.node_id);
  }

  function removeSelectedNode() {
    if (!selectedNodeId) return;
    setDefinition((current) => ({
      ...current,
      nodes: current.nodes.filter((node) => node.node_id !== selectedNodeId).map((node) => ({
        ...node,
        inputs: Object.fromEntries(Object.entries(node.inputs).filter(([, binding]) => !(binding.source === "node_output" && binding.node_id === selectedNodeId))),
        permission_policy: { ...node.permission_policy, approval_node_id: node.permission_policy.approval_node_id === selectedNodeId ? null : node.permission_policy.approval_node_id },
        guard: node.guard?.condition_node_id === selectedNodeId ? null : node.guard,
      })),
      outputs: Object.fromEntries(Object.entries(current.outputs).filter(([, output]) => !(output.binding.source === "node_output" && output.binding.node_id === selectedNodeId))),
      secrets: Object.fromEntries(Object.entries(current.secrets).map(([id, secret]) => [id, { ...secret, allowed_node_ids: secret.allowed_node_ids.filter((nodeId) => nodeId !== selectedNodeId) }])),
    }));
    setSelectedNodeId(null);
  }

  function addEdge() {
    if (!edge.sourceNode || !edge.targetNode || !edge.sourcePort || !edge.targetPort || edge.sourceNode === edge.targetNode) return;
    setDefinition((current) => ({
      ...current,
      nodes: current.nodes.map((node) => node.node_id === edge.targetNode ? {
        ...node,
        inputs: { ...node.inputs, [edge.targetPort]: { source: "node_output", node_id: edge.sourceNode, port: edge.sourcePort } },
      } : node),
    }));
    setSelectedNodeId(edge.targetNode);
  }

  function removeEdge(nodeId: string, port: string) {
    setDefinition((current) => ({
      ...current,
      nodes: current.nodes.map((node) => node.node_id === nodeId ? { ...node, inputs: Object.fromEntries(Object.entries(node.inputs).filter(([name]) => name !== port)) } : node),
    }));
  }

  async function validate() {
    const draft = applyDraft();
    if (!draft) return;
    try { await validateWorkflow(draft); setLocalError(null); } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function save() {
    const draft = applyDraft();
    if (!draft) return;
    const saving = exists ? { ...draft, workflow_version: definition.workflow_version + 1 } : draft;
    try {
      await saveWorkflow(saving, exists);
      loadDefinition(saving);
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function importLegacy() {
    try {
      const ir = await ecosystemClient.importLegacyWorkflow(JSON.parse(legacyText) as LegacyRecipeV1);
      await refreshWorkflows();
      const imported = await ecosystemClient.loadWorkflow(ir.workflow_id);
      loadDefinition(imported);
      setShowLegacy(false);
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function prepareApproval(node: WorkflowNode) {
    try {
      const challenge = await ecosystemClient.prepareWorkflowApproval(definition.workflow_id, runId, node.node_id, approvalSummary(node));
      setApprovalChallenges((current) => ({ ...current, [node.node_id]: challenge }));
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function decideApproval(nodeId: string, approved: boolean) {
    const challenge = approvalChallenges[nodeId];
    if (!challenge) return;
    try {
      const chainApproved = await ecosystemClient.decideWorkflowApproval(challenge.challenge_id, approved);
      setApprovalChallenges((current) => { const next = { ...current }; delete next[nodeId]; return next; });
      setApprovedNodes((current) => { const next = new Set(current); if (chainApproved) next.add(nodeId); else next.delete(nodeId); return next; });
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function run() {
    try {
      const request: WorkflowRunRequest = {
        run_id: runId,
        inputs: JSON.parse(runInputsText),
        secret_bindings: JSON.parse(secretBindingsText),
        trigger: JSON.parse(runTriggerText) as WorkflowTrigger,
      };
      await runWorkflow(definition.workflow_id, request);
      setRunId(newRunId());
      setApprovedNodes(new Set());
      setApprovalChallenges({});
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function registerTriggers() {
    try {
      const ids = await ecosystemClient.registerWorkflowTriggers(definition.workflow_id);
      setTriggerIds(ids);
      setTriggerStatus(ids.length > 0 ? t("EcosystemWorkflow.triggersEnabled", { count: ids.length }) : t("EcosystemWorkflow.noPersistentTriggers"));
      setLocalError(null);
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function unregisterTriggers() {
    try {
      await ecosystemClient.unregisterWorkflowTriggers(definition.workflow_id);
      setTriggerIds([]);
      setTriggerStatus(t("EcosystemWorkflow.triggersDisabled"));
      setLocalError(null);
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  const humanApprovalNodes = definition.nodes.filter((node) => node.kind.kind === "human_approval");
  const edges = definition.nodes.flatMap((node) => Object.entries(node.inputs).flatMap(([port, binding]) => binding.source === "node_output" ? [{ source: binding.node_id, sourcePort: binding.port, target: node.node_id, targetPort: port }] : []));
  const persisted = workflows.find((workflow) => workflow.workflow_id === definition.workflow_id);

  return (
    <div className="space-y-5">
      <div className="grid gap-4 lg:grid-cols-[15rem_minmax(0,1fr)]">
        <aside className="space-y-3">
          <div className="flex gap-2"><Button size="sm" variant="primary" className="flex-1" onClick={() => loadDefinition(newWorkflowDefinition())}><Plus size={14} />{t("EcosystemWorkflow.new")}</Button><Button size="sm" title={t("EcosystemWorkflow.refresh")} onClick={() => void refreshWorkflows()}><RefreshCw size={14} /></Button></div>
          <div className="max-h-72 space-y-1 overflow-y-auto rounded-xl border border-border bg-surface p-2">
            {workflows.map((workflow) => <button key={workflow.workflow_id} type="button" onClick={() => loadDefinition(workflow)} className={`w-full rounded-lg px-2.5 py-2 text-left ${workflow.workflow_id === definition.workflow_id ? "bg-accent-soft text-foreground" : "text-muted hover:bg-surface-2 hover:text-foreground"}`}><span className="block truncate text-xs font-medium">{workflow.name}</span><span className="mt-0.5 block truncate font-mono text-[10px] text-faint">{workflow.workflow_id} · v{workflow.workflow_version}</span></button>)}
            {workflows.length === 0 && <p className="p-4 text-center text-xs text-muted">{t("EcosystemWorkflow.noSaved")}</p>}
          </div>
          <Button size="sm" variant="ghost" className="w-full" onClick={() => setShowLegacy((value) => !value)}><Import size={14} />{t("EcosystemWorkflow.importLegacy")}</Button>
          {showLegacy && <div className="space-y-2"><textarea rows={12} className={AREA} value={legacyText} onChange={(event) => setLegacyText(event.target.value)} spellCheck={false} /><Button size="sm" variant="primary" className="w-full" onClick={() => void importLegacy()}>{t("EcosystemWorkflow.import")}</Button></div>}
        </aside>

        <div className="min-w-0 space-y-4">
          <section className="rounded-xl border border-border bg-surface p-4">
            <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_7rem]">
              <label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.name")}</span><input className={FIELD} value={definition.name} onChange={(event) => setDefinition((current) => ({ ...current, name: event.target.value }))} /></label>
              <label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.id")}</span><input className={FIELD} value={definition.workflow_id} disabled={exists} onChange={(event) => setDefinition((current) => ({ ...current, workflow_id: event.target.value }))} /></label>
              <label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.version")}</span><input type="number" min={1} className={FIELD} value={definition.workflow_version} disabled={exists} onChange={(event) => setDefinition((current) => ({ ...current, workflow_version: Number(event.target.value) }))} /></label>
            </div>
            <div className="mt-3 flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap gap-2">
                <select value={addKind} onChange={(event) => setAddKind(event.target.value as AddableNodeKind)} className={FIELD + " !w-auto"}>{NODE_KINDS.map((kind) => <option key={kind} value={kind}>{kind}</option>)}</select>
                <Button size="sm" onClick={addNode}><Plus size={14} />{t("EcosystemWorkflow.addNode")}</Button>
              </div>
              <div className="flex flex-wrap gap-2"><Button size="sm" disabled={busy["workflow-validate"]} onClick={() => void validate()}><Check size={14} />{t("EcosystemWorkflow.validate")}</Button><Button size="sm" variant="primary" disabled={busy["workflow-save"]} onClick={() => void save()}><Save size={14} />{exists ? t("EcosystemWorkflow.saveNewVersion") : t("EcosystemWorkflow.create")}</Button>{exists && (confirmDelete ? <span className="inline-flex items-center gap-1 rounded-lg border border-danger/30 bg-danger-soft p-1"><Button size="sm" variant="danger" onClick={() => void deleteWorkflow(definition.workflow_id).then(() => loadDefinition(newWorkflowDefinition()))}>{t("EcosystemWorkflow.confirmDelete")}</Button><Button size="sm" variant="ghost" onClick={() => setConfirmDelete(false)}>{t("EcosystemWorkflow.cancel")}</Button></span> : <Button size="sm" variant="danger" onClick={() => setConfirmDelete(true)}><Trash2 size={14} />{t("EcosystemWorkflow.delete")}</Button>)}</div>
            </div>
          </section>

          <WorkflowDag definition={definition} selectedNodeId={selectedNodeId} onSelect={setSelectedNodeId} />

          <section className="grid gap-4 xl:grid-cols-2">
            <div className="rounded-xl border border-border bg-surface p-4">
              <div className="flex items-center justify-between"><h3 className="text-sm font-semibold text-foreground">{t("EcosystemWorkflow.nodeConfiguration")}</h3>{selectedNode && <Button size="sm" variant="ghost" onClick={removeSelectedNode}><Trash2 size={13} />{t("EcosystemWorkflow.removeNode")}</Button>}</div>
              {selectedNode ? <div className="mt-3 space-y-3">
                <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.nodeId")}</span><input className={FIELD} value={nodeIdText} onChange={(event) => setNodeIdText(event.target.value)} /></label>
                {selectedAdapterActions && (selectedNode.kind.kind === "browser" || selectedNode.kind.kind === "git" || selectedNode.kind.kind === "pull_request") && <label className="block text-xs text-muted">
                  <span className="mb-1 block">Adapter action</span>
                  <select className={FIELD} value={selectedNode.kind.action} onChange={(event) => setAdapterAction(event.target.value)}>
                    {selectedAdapterActions.map(([action, effect]) => <option key={action} value={action}>{action} · {effect}</option>)}
                  </select>
                  <span className="mt-1 block text-[10px] text-faint">Mutation actions automatically add and bind a human-approval node.</span>
                </label>}
                {(selectedNode.kind.kind === "agent" || selectedNode.kind.kind === "subagent") && <label className="block text-xs text-muted">
                  <span className="mb-1 block">Agent profile</span>
                  <select className={FIELD} value={selectedNode.kind.agent_profile} onChange={(event) => setAgentProfile(event.target.value)}><option value="default">default</option><option value="explore">explore</option><option value="review">review</option></select>
                </label>}
                {selectedMcpKind && <div className="grid gap-2 sm:grid-cols-2">
                  <label className="text-xs text-muted"><span className="mb-1 block">MCP server</span><select className={FIELD} value={selectedMcpKind.server_id} onChange={(event) => setMcpTarget(event.target.value)}><option value="server">Choose server</option>{mcpServers.filter((server) => server.enabled && (server.toolAllowlist?.length ?? 0) > 0).map((server) => <option key={server.id} value={server.id}>{server.label}</option>)}</select></label>
                  <label className="text-xs text-muted"><span className="mb-1 block">Allowlisted tool</span><select className={FIELD} value={selectedMcpKind.tool_name} onChange={(event) => setMcpTarget(selectedMcpKind.server_id, event.target.value)}>{(mcpServers.find((server) => server.id === selectedMcpKind.server_id)?.toolAllowlist ?? [selectedMcpKind.tool_name]).map((tool) => <option key={tool} value={tool}>{tool}</option>)}</select></label>
                  <p className="text-[10px] text-faint sm:col-span-2">MCP tools are conservatively treated as external mutations and require an approval node.</p>
                </div>}
                <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.nodeKindJson")}</span><textarea rows={6} className={AREA} value={nodeKindText} onChange={(event) => setNodeKindText(event.target.value)} spellCheck={false} /></label>
                <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.nodeInputsJson")}</span><textarea rows={8} className={AREA} value={nodeInputsText} onChange={(event) => setNodeInputsText(event.target.value)} spellCheck={false} /></label>
                <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.nodePolicyJson")}</span><textarea rows={10} className={AREA} value={nodeAdvancedText} onChange={(event) => setNodeAdvancedText(event.target.value)} spellCheck={false} /></label>
                <Button size="sm" onClick={applyDraft}><Code2 size={14} />{t("EcosystemWorkflow.applyNode")}</Button>
              </div> : <p className="mt-4 text-xs text-muted">{t("EcosystemWorkflow.selectNode")}</p>}
            </div>

            <div className="space-y-4">
              <div className="rounded-xl border border-border bg-surface p-4">
                <h3 className="text-sm font-semibold text-foreground">{t("EcosystemWorkflow.edges")}</h3>
                <div className="mt-3 grid grid-cols-2 gap-2">
                  <select className={FIELD} value={edge.sourceNode} onChange={(event) => setEdge((current) => ({ ...current, sourceNode: event.target.value }))}><option value="">{t("EcosystemWorkflow.sourceNode")}</option>{definition.nodes.map((node) => <option key={node.node_id} value={node.node_id}>{node.node_id}</option>)}</select>
                  <input className={FIELD} value={edge.sourcePort} aria-label={t("EcosystemWorkflow.sourcePort")} onChange={(event) => setEdge((current) => ({ ...current, sourcePort: event.target.value }))} />
                  <select className={FIELD} value={edge.targetNode} onChange={(event) => setEdge((current) => ({ ...current, targetNode: event.target.value }))}><option value="">{t("EcosystemWorkflow.targetNode")}</option>{definition.nodes.map((node) => <option key={node.node_id} value={node.node_id}>{node.node_id}</option>)}</select>
                  <input className={FIELD} value={edge.targetPort} aria-label={t("EcosystemWorkflow.targetPort")} onChange={(event) => setEdge((current) => ({ ...current, targetPort: event.target.value }))} />
                </div>
                <Button size="sm" className="mt-2" onClick={addEdge}><GitBranch size={14} />{t("EcosystemWorkflow.connect")}</Button>
                <div className="mt-3 space-y-1.5">{edges.map((item) => <div key={`${item.target}:${item.targetPort}`} className="flex items-center gap-1.5 rounded bg-surface-2 px-2 py-1.5 text-[11px] text-muted"><span className="truncate">{item.source}.{item.sourcePort}</span><ChevronRight size={11} className="shrink-0" /><span className="truncate">{item.target}.{item.targetPort}</span><button type="button" className="ml-auto text-danger hover:underline" onClick={() => removeEdge(item.target, item.targetPort)}>{t("EcosystemWorkflow.remove")}</button></div>)}</div>
              </div>
              <div className="rounded-xl border border-border bg-surface p-4"><h3 className="text-sm font-semibold text-foreground">{t("EcosystemWorkflow.definitionConfiguration")}</h3><p className="mt-1 text-xs text-muted">{t("EcosystemWorkflow.definitionDescription")}</p><textarea rows={18} className={`${AREA} mt-3`} value={definitionText} onChange={(event) => setDefinitionText(event.target.value)} spellCheck={false} /><Button size="sm" className="mt-2" onClick={applyDraft}>{t("EcosystemWorkflow.applyDefinition")}</Button></div>
            </div>
          </section>

          {(localError || workflowIr) && <section className={`rounded-xl border p-4 ${localError ? "border-danger/30 bg-danger-soft" : "border-success/30 bg-success-soft"}`}>{localError ? <div className="flex items-start gap-2 text-xs text-danger"><AlertTriangle size={15} className="shrink-0" /><pre className="whitespace-pre-wrap font-sans">{localError}</pre></div> : workflowIr && <div><div className="flex items-center gap-2 text-sm font-semibold text-success"><Check size={15} />{t("EcosystemWorkflow.valid")}</div><p className="mt-1 break-all font-mono text-[11px] text-muted">sha256 {workflowIr.definition_sha256}</p><div className="mt-2 flex flex-wrap gap-1.5">{workflowIr.nodes.map((node) => <span key={node.node.node_id} className="rounded bg-background/60 px-2 py-1 text-[11px] text-muted">L{node.level} · {node.node.node_id}</span>)}</div></div>}</section>}

          <section className="rounded-xl border border-border bg-surface p-4">
            <div className="flex flex-wrap items-start justify-between gap-3"><div><h3 className="text-sm font-semibold text-foreground">{t("EcosystemWorkflow.runTitle")}</h3><p className="mt-1 text-xs text-muted">{t("EcosystemWorkflow.runDescription")}</p></div>{activeRunId && <Button size="sm" variant="danger" onClick={() => void cancelWorkflow(activeRunId)}><CircleStop size={14} />{t("EcosystemWorkflow.cancelRun")}</Button>}</div>
            <label className="mt-3 block text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.runId")}</span><input className={FIELD} value={runId} onChange={(event) => { setRunId(event.target.value); setApprovedNodes(new Set()); setApprovalChallenges({}); }} /></label>
            <div className="mt-3 grid gap-3 lg:grid-cols-3"><label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.inputsJson")}</span><textarea rows={9} className={AREA} value={runInputsText} onChange={(event) => setRunInputsText(event.target.value)} /></label><label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.secretRefsJson")}</span><textarea rows={9} className={AREA} value={secretBindingsText} onChange={(event) => setSecretBindingsText(event.target.value)} /></label><label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemWorkflow.triggerJson")}</span><textarea rows={9} className={AREA} value={runTriggerText} onChange={(event) => setRunTriggerText(event.target.value)} /></label></div>
            {humanApprovalNodes.length > 0 && <div className="mt-4 space-y-2 rounded-lg border border-warning/30 bg-warning-soft p-3"><h4 className="text-xs font-semibold text-foreground">{t("EcosystemWorkflow.approvalsTitle")}</h4>{humanApprovalNodes.map((node) => { const challenge = approvalChallenges[node.node_id]; const approved = approvedNodes.has(node.node_id); return <div key={node.node_id} className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-background/50 p-2.5 text-xs"><div><p className="font-medium text-foreground">{node.node_id}</p><p className="mt-0.5 text-muted">{approvalSummary(node)}</p></div>{approved ? <StatusPill tone="success">{t("EcosystemWorkflow.approvedOnce")}</StatusPill> : challenge ? <div className="flex gap-2"><Button size="sm" variant="ghost" onClick={() => void decideApproval(node.node_id, false)}>{t("EcosystemWorkflow.deny")}</Button><Button size="sm" variant="primary" onClick={() => void decideApproval(node.node_id, true)}>{t("EcosystemWorkflow.approveOnce")}</Button></div> : <Button size="sm" onClick={() => void prepareApproval(node)}>{t("EcosystemWorkflow.reviewApproval")}</Button>}</div>; })}</div>}
            <div className="mt-4 flex flex-wrap justify-end gap-2"><Button disabled={!persisted} onClick={() => void registerTriggers()}><ServerCog size={14} />{t("EcosystemWorkflow.enablePersistentTriggers")}</Button><Button disabled={!persisted} variant="ghost" onClick={() => void unregisterTriggers()}>{t("EcosystemWorkflow.disablePersistentTriggers")}</Button><Button variant="primary" disabled={!persisted || Boolean(activeRunId) || humanApprovalNodes.some((node) => !approvedNodes.has(node.node_id))} onClick={() => void run()}><Play size={14} />{t("EcosystemWorkflow.run")}</Button></div>
            {triggerStatus && <p role="status" className="mt-2 text-xs text-muted">{triggerStatus}</p>}
            {triggerIds.length > 0 && <div className="mt-3 rounded-lg bg-surface-2 p-3"><p className="text-xs font-medium text-foreground">{t("EcosystemWorkflow.triggerIds")}</p>{triggerIds.map((id) => <code key={id} className="mt-1 block break-all text-[11px] text-muted">{id}</code>)}</div>}
          </section>
        </div>
      </div>
    </div>
  );
}

export function EcosystemWorkflowRuns() {
  const { t } = useT();
  const { histories, inspectedNode, busy, refreshHistories, inspectNode } = useEcosystemStore();
  const [selectedRunId, setSelectedRunId] = useState<string | null>(histories[0]?.run_id ?? null);
  const [boundaryNodeId, setBoundaryNodeId] = useState("");
  const [replayApproval, setReplayApproval] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const selected = histories.find((history) => history.run_id === selectedRunId) ?? histories[0] ?? null;

  useEffect(() => { if (!selectedRunId && histories[0]) setSelectedRunId(histories[0].run_id); }, [histories, selectedRunId]);

  async function reconcile(nodeId: string, decision: "verified_applied" | "verified_not_applied" | "abandon") {
    try { await ecosystemClient.reconcileWorkflowNode(selected!.run_id, nodeId, decision); await refreshHistories(); } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  async function replay() {
    if (!selected || !boundaryNodeId) return;
    try {
      const request: WorkflowRunRequest = { run_id: newRunId("replay"), inputs: selected.input_snapshot, secret_bindings: selected.secret_reference_snapshot, trigger: selected.trigger };
      await ecosystemClient.replayWorkflow(selected.workflow_id, selected.run_id, boundaryNodeId, replayApproval, request);
      await refreshHistories();
    } catch (error) { setLocalError(error instanceof Error ? error.message : String(error)); }
  }

  return (
    <div className="grid gap-4 lg:grid-cols-[18rem_minmax(0,1fr)]">
      <aside className="space-y-2">
        <div className="flex items-center justify-between"><h3 className="text-sm font-semibold text-foreground">{t("EcosystemRuns.history")}</h3><Button size="sm" variant="ghost" disabled={busy.histories} onClick={() => void refreshHistories()}><RefreshCw size={14} /></Button></div>
        <div className="max-h-[65vh] space-y-2 overflow-y-auto pr-1">{histories.map((history) => <button key={history.run_id} type="button" onClick={() => { setSelectedRunId(history.run_id); setBoundaryNodeId(""); }} className={`w-full rounded-xl border p-3 text-left ${selected?.run_id === history.run_id ? "border-accent bg-accent-soft" : "border-border bg-surface hover:border-border-strong"}`}><div className="flex items-center justify-between gap-2"><span className="truncate text-xs font-semibold text-foreground">{history.workflow_id}</span><StatusPill tone={statusTone(history.status)}>{history.status}</StatusPill></div><p className="mt-1 truncate font-mono text-[10px] text-muted">{history.run_id}</p><p className="mt-1 text-[10px] text-faint">{new Date(history.started_unix_ms).toLocaleString()}</p></button>)}</div>
        {histories.length === 0 && <div className="rounded-xl border border-dashed border-border p-6 text-center text-xs text-muted">{t("EcosystemRuns.empty")}</div>}
      </aside>

      <div className="min-w-0 space-y-4">
        {selected ? <>
          <section className="rounded-xl border border-border bg-surface p-4"><div className="flex flex-wrap items-start justify-between gap-3"><div><h3 className="text-sm font-semibold text-foreground">{selected.workflow_id}</h3><p className="mt-1 break-all font-mono text-[11px] text-muted">{selected.run_id}</p></div><StatusPill tone={statusTone(selected.status)}>{selected.status}</StatusPill></div><dl className="mt-4 grid gap-3 text-xs sm:grid-cols-3"><div><dt className="text-faint">{t("EcosystemRuns.started")}</dt><dd className="mt-1 text-foreground">{new Date(selected.started_unix_ms).toLocaleString()}</dd></div><div><dt className="text-faint">{t("EcosystemRuns.finished")}</dt><dd className="mt-1 text-foreground">{selected.finished_unix_ms ? new Date(selected.finished_unix_ms).toLocaleString() : "—"}</dd></div><div><dt className="text-faint">{t("EcosystemRuns.digest")}</dt><dd className="mt-1 truncate font-mono text-foreground" title={selected.definition_sha256}>{selected.definition_sha256}</dd></div></dl></section>
          <section className="rounded-xl border border-border bg-surface p-4"><h3 className="flex items-center gap-2 text-sm font-semibold text-foreground"><History size={15} />{t("EcosystemRuns.nodes")}</h3><div className="mt-3 space-y-2">{Object.values(selected.nodes).map((node) => { const status = node.status.status; const needsReconciliation = status === "needs_reconciliation"; return <article key={node.node_id} className="rounded-lg border border-border bg-surface-2 p-3"><div className="flex flex-wrap items-center justify-between gap-2"><div><p className="font-mono text-xs font-medium text-foreground">{node.node_id}</p><p className="mt-1 text-[11px] text-muted">{status} · {node.attempts} {t("EcosystemRuns.attempts")}</p></div><Button size="sm" onClick={() => void inspectNode(selected.run_id, node.node_id)}>{t("EcosystemRuns.inspect")}</Button></div>{needsReconciliation && <div className="mt-3 flex flex-wrap gap-2 border-t border-border pt-3"><Button size="sm" onClick={() => void reconcile(node.node_id, "verified_applied")}>{t("EcosystemRuns.verifiedApplied")}</Button><Button size="sm" onClick={() => void reconcile(node.node_id, "verified_not_applied")}>{t("EcosystemRuns.verifiedNotApplied")}</Button><Button size="sm" variant="danger" onClick={() => void reconcile(node.node_id, "abandon")}>{t("EcosystemRuns.abandon")}</Button></div>}</article>; })}</div></section>
          <section className="rounded-xl border border-border bg-surface p-4"><h3 className="flex items-center gap-2 text-sm font-semibold text-foreground"><ListRestart size={15} />{t("EcosystemRuns.replay")}</h3><p className="mt-1 text-xs text-muted">{t("EcosystemRuns.replayDescription")}</p><div className="mt-3 flex flex-wrap items-end gap-2"><label className="min-w-56 flex-1 text-xs text-muted"><span className="mb-1 block">{t("EcosystemRuns.boundary")}</span><select className={FIELD} value={boundaryNodeId} onChange={(event) => setBoundaryNodeId(event.target.value)}><option value="">{t("EcosystemRuns.chooseNode")}</option>{Object.keys(selected.nodes).map((nodeId) => <option key={nodeId} value={nodeId}>{nodeId}</option>)}</select></label><label className="flex h-9 items-center gap-2 text-xs text-muted"><input type="checkbox" checked={replayApproval} onChange={(event) => setReplayApproval(event.target.checked)} className="h-4 w-4 accent-accent" />{t("EcosystemRuns.replayApproval")}</label><Button variant="primary" disabled={!boundaryNodeId} onClick={() => void replay()}><ListRestart size={14} />{t("EcosystemRuns.startReplay")}</Button></div></section>
          <section className="grid gap-4 xl:grid-cols-2"><div className="rounded-xl border border-border bg-surface p-4"><h3 className="text-sm font-semibold text-foreground">{t("EcosystemRuns.outputs")}</h3><pre className="mt-3 max-h-72 overflow-auto rounded-lg bg-surface-2 p-3 text-xs text-foreground">{JSON.stringify(selected.outputs, null, 2)}</pre></div><div className="rounded-xl border border-border bg-surface p-4"><h3 className="text-sm font-semibold text-foreground">{t("EcosystemRuns.nodeInspection")}</h3><pre className="mt-3 max-h-72 overflow-auto rounded-lg bg-surface-2 p-3 text-xs text-foreground">{inspectedNode ? JSON.stringify(inspectedNode, null, 2) : t("EcosystemRuns.selectInspect")}</pre></div></section>
        </> : <div className="flex min-h-72 flex-col items-center justify-center rounded-xl border border-dashed border-border text-center text-sm text-muted"><Workflow size={28} className="mb-3 text-faint" />{t("EcosystemRuns.selectRun")}</div>}
        {localError && <div className="flex items-start gap-2 rounded-lg border border-danger/30 bg-danger-soft p-3 text-xs text-danger"><AlertTriangle size={14} className="shrink-0" />{localError}</div>}
      </div>
    </div>
  );
}
