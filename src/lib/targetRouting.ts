/**
 * Which model target runs a piece of work — resolution, and the K9 dispatch
 * policy that may change it.
 *
 * Lifted out of `agentLoop.ts` verbatim rather than reimplemented. The move is
 * the point: `subagent.ts` must not import `agentLoop.ts` — `turnEngine.ts`
 * imports `subagent.ts`, so that edge would close a cycle — and the rule was
 * load-bearing enough that subagents simply did not route at all, which is the
 * gap K9's entry names. Nothing here needs the agent loop; everything it needs
 * is a store or a pure function, so the dependency was never real, only
 * co-located.
 *
 * `agentLoop.ts` re-exports the four names its own callers already import
 * (`resolveTarget`, `snapshotForResolvedTarget`, `routeFromActive`,
 * `routeTarget`), so the ~70 modules that read target resolution through it are
 * untouched by the move.
 *
 * The one runtime import from `turnEngine.ts` would re-close the cycle, so
 * `ResolvedTarget` is imported as a type: erased at compile time, no edge at
 * runtime.
 */
import { invoke } from '@tauri-apps/api/core';

import type { ResolvedTarget } from './turnEngine';
import {
  isVisionCapableLocalModel,
  isVisionCapableOllamaModel,
  isVisionCapableProviderModel,
} from './visionModels';
import {
  buildModelTargetInventory,
  type ModelTargetInventory,
  type ModelTargetSnapshot,
} from './modelTargets';
import {
  observedTimeToFirstTokenMs,
  routeRequest,
  type RoutingCandidate,
  type RoutingDecision,
  type RoutingTaskClass,
} from './modelRouting';
import { getActiveChatTarget, useModelStore } from '../store/modelStore';
import { useCostControlStore } from '../store/costControlStore';
import { useRoutingPolicyStore } from '../store/routingPolicyStore';

/** Mirrors `LlamaState::default()` in src-tauri/src/llama.rs. */
const DEFAULT_LLAMA_PORT = 8090;

/** Shape returned by the `llama_status` Tauri command. */
interface LlamaStatusPayload {
  status: 'stopped' | 'starting' | 'ready' | 'error';
  port: number;
  model_path: string | null;
}

/**
 * Resolves the base URL of the locally running llama-server by asking the
 * Rust backend for its current status, which is the source of truth for the
 * port it actually bound to. Falls back to the documented default port if
 * the status can't be read for any reason (e.g. server not started yet),
 * so a subsequent request simply fails with a clear connection error rather
 * than this function throwing before the user ever sees why.
 */
async function resolveBaseUrl(): Promise<string> {
  try {
    const status = await invoke<LlamaStatusPayload>('llama_status');
    // `llama_status` is the native source of truth. Keep the model store in
    // step with it before the target inventory is frozen; otherwise a server
    // that is already ready can still look stopped to the chat path during
    // startup or immediately after a model switch.
    const installed = useModelStore.getState().installed;
    useModelStore.setState((state) => ({
      llamaStatus: status.status,
      active: status.model_path
        ? installed.find((model) => model.path === status.model_path) ?? state.active
        : state.active,
    }));
    const port =
      typeof status?.port === 'number' && Number.isFinite(status.port) && status.port > 0
        ? status.port
        : DEFAULT_LLAMA_PORT;
    return `http://127.0.0.1:${port}`;
  } catch {
    return `http://127.0.0.1:${DEFAULT_LLAMA_PORT}`;
  }
}

/**
 * Resolves the active chat target into exactly what's needed to stream a
 * turn. Cloud providers go through the Rust-proxied `streamProviderChat`
 * (its API key lives in the OS keychain, never here); local llama.cpp and
 * the unauthenticated local Ollama daemon both use the direct-`fetch`
 * `streamChat` path.
 *
 * Exported so `sideTaskRunner.ts` can default a newly-started side task onto
 * whatever model the main chat is currently using, without re-implementing
 * this resolution logic (or the local-runtime `resolveBaseUrl` lookup a
 * from-scratch version would need) a second time — a side task's own loop is
 * deliberately independent of the parent turn (see that module's doc
 * comment), but "which model is active right now" is still a single, shared
 * piece of app state both should read the same way.
 */
/** Exported (in addition to the module's own uses above) so
 * `issueToPrRunner.ts` can resolve the exact same active-target rules for
 * its own headless, panel-driven agent turn — see that module's doc comment
 * for why it reuses this rather than re-deriving target resolution. */
export async function resolveTarget(): Promise<ResolvedTarget> {
  const target = getActiveChatTarget();

  if (target.kind === 'provider') {
    if (!target.providerId || !target.model) {
      throw new Error('No AI provider model selected');
    }
    const resolved = { kind: 'provider', providerId: target.providerId, model: target.model } as const;
    await refreshTargetInventoryIfMissing(resolved);
    return resolved;
  }

  if (target.kind === 'ollama') {
    if (!target.model) {
      throw new Error('No Ollama model selected');
    }
    const resolved = { kind: 'ollama', baseUrl: target.baseUrl, model: target.model } as const;
    await refreshTargetInventoryIfMissing(resolved);
    return resolved;
  }

  const baseUrl = await resolveBaseUrl();
  const resolved = { kind: 'local', baseUrl, modelLabel: useModelStore.getState().active?.name ?? 'Local model' } as const;
  await refreshTargetInventoryIfMissing(resolved);
  return resolved;
}

/** Reconcile a stale in-memory model inventory before a resident turn freezes
 * its execution target. The normal path remains synchronous; this is only a
 * recovery pass for startup, model-switch, or daemon-refresh races. */
async function refreshTargetInventoryIfMissing(target: ResolvedTarget): Promise<void> {
  if (snapshotForResolvedTarget(target)) return;
  try {
    if (target.kind === 'local') {
      await useModelStore.getState().refresh();
    } else if (target.kind === 'ollama') {
      await useModelStore.getState().refreshOllama();
    } else {
      await useModelStore.getState().refreshProviderModels(target.providerId);
    }
  } catch {
    // The caller still performs the final snapshot check and reports the
    // target-specific failure; inventory refresh is best effort here.
  }
}

/** Resolves a streaming target back to the immutable inventory record that
 * contains its endpoint/model/capability evidence. The inventory is built
 * once at run start; later global model changes cannot rewrite the ledger
 * snapshot. */
/** Exported so `issueToPrRunner.ts` can build the `target` field
 * `beginDurableRun` needs for its own headless run, the same reuse reasoning
 * as `resolveTarget` above. */
/** The live target inventory — the same set the model picker offers. Shared by
 * `snapshotForResolvedTarget` and the K9 routing candidates below so a target
 * routing can choose is by construction one the user already configured. */
export function currentTargetInventory(): ModelTargetInventory {
  const state = useModelStore.getState();
  return buildModelTargetInventory({
    installed: state.installed,
    active: state.active,
    llamaStatus: state.llamaStatus,
    ollamaModels: state.ollamaModels,
    ollamaReachable: state.ollamaReachable,
    providers: state.providers,
    providerModels: state.providerModels,
    effortByTarget: state.effortByTarget,
  });
}

export function snapshotForResolvedTarget(target: ResolvedTarget): ModelTargetSnapshot | null {
  const inventory = currentTargetInventory();
  if (target.kind === 'local') {
    return inventory.targets.find((candidate) => candidate.kind === 'local') ?? null;
  }
  if (target.kind === 'ollama') {
    return inventory.targets.find(
      (candidate) => candidate.kind === 'ollama' && candidate.model === target.model,
    ) ?? null;
  }
  return inventory.targets.find(
    (candidate) =>
      candidate.kind === 'provider' &&
      candidate.providerId === target.providerId &&
      candidate.model === target.model,
  ) ?? null;
}

/** Human-readable label for a switch notice. */
export function targetLabel(target: ResolvedTarget): string {
  if (target.kind === 'provider') return `${target.providerId} (${target.model})`;
  if (target.kind === 'ollama') return `Ollama (${target.model})`;
  return 'the local model';
}

/** Applies `target` as the app's active chat target — the same store setters a manual switch in the UI would call, which is exactly what makes the switch "sticky" across subsequent turns (session affinity) with no separate mechanism needed. */
export function applyTargetSwitch(target: ResolvedTarget): void {
  if (target.kind === 'provider') {
    useModelStore.getState().useProviderModel(target.providerId, target.model);
  } else if (target.kind === 'ollama') {
    useModelStore.getState().useOllamaModel(target.model);
  }
  // 'local' is never produced as a switch target — see buildFailoverChain/findVisionCandidate.
}

/** Whether `target` (an already-*resolved* target, unlike `activeTargetSatisfiesVision`
 * below which reads live store state) can see images — used to decide whether
 * older image-bearing turns still in `history` need stripping before this
 * particular target sees them. See `stripImagesForTextOnlyTarget`. */
export function resolvedTargetSupportsVision(target: ResolvedTarget): boolean {
  if (target.kind === 'provider') return isVisionCapableProviderModel(target.providerId, target.model);
  if (target.kind === 'ollama') {
    const model = useModelStore.getState().ollamaModels.find((m) => m.name === target.model);
    return model ? isVisionCapableOllamaModel(model) : false;
  }
  // Deliberately delegated (rather than a literal `false`) so the day
  // `llama.rs` gains projector-backed vision chat, flipping
  // `isVisionCapableLocalModel` updates every vision decision at once.
  return isVisionCapableLocalModel();
}

/** Turns one inventory snapshot into a routing candidate.
 *
 * Vision goes through `resolvedTargetSupportsVision` — the same predicate the
 * turn itself uses — rather than the snapshot's own field, which is `unknown`
 * for every provider model (`modelTargets.ts::providerTarget`) while
 * `visionModels.ts` name patterns plus the user's overrides are what the rest
 * of the loop actually believes. Deciding it any other way could route an
 * image to a model `stripImagesForTextOnlyTarget` then strips it for.
 */
export function routingCandidate(snapshot: ModelTargetSnapshot): RoutingCandidate {
  const rates = useCostControlStore.getState().rates[snapshot.key];
  const entries = useCostControlStore.getState().entries;
  const isLocal = snapshot.kind === 'ollama' ? snapshot.isCloud !== true : snapshot.kind === 'local';
  const vision = resolvedTargetSupportsVision(
    snapshot.kind === 'provider'
      ? { kind: 'provider', providerId: snapshot.providerId, model: snapshot.model }
      : snapshot.kind === 'ollama'
        ? { kind: 'ollama', baseUrl: snapshot.baseUrl, model: snapshot.model }
        : { kind: 'local', baseUrl: '', modelLabel: snapshot.displayName },
  );
  return {
    key: snapshot.key,
    label: `${snapshot.label} · ${snapshot.displayName}`,
    isLocal,
    available: snapshot.availability.status === 'available',
    toolCalling: snapshot.capabilities.toolCalling.state,
    vision: vision ? 'yes' : 'no',
    // A target that costs nothing to run has a rate of zero, not an unknown
    // one — otherwise every cost ceiling would exclude local models, which is
    // the opposite of what a cost-conscious policy wants.
    inputPerMillionUsd: isLocal ? 0 : rates?.inputPerMillionUsd ?? null,
    outputPerMillionUsd: isLocal ? 0 : rates?.outputPerMillionUsd ?? null,
    observedTimeToFirstTokenMs: observedTimeToFirstTokenMs(entries, snapshot.key),
  };
}

/** Converts an inventory snapshot back into a streamable target.
 *
 * ponytail: managed llama.cpp is deliberately not routable — the same
 * reasoning `buildFailoverChain` and `findLocalOnlyTarget` already document
 * (making it the active target is not something an automatic switch has a
 * basis to do unattended), so it is excluded from candidates below and
 * `local_only` policies are served by non-cloud Ollama exactly as the Privacy
 * Firewall's own local fallback is. Upgrade path if it is ever wanted: teach
 * `applyTargetSwitch` the `local` kind and drop the filter.
 */
export function resolvedFromSnapshot(snapshot: ModelTargetSnapshot): ResolvedTarget | null {
  if (snapshot.kind === 'provider') {
    return { kind: 'provider', providerId: snapshot.providerId, model: snapshot.model };
  }
  if (snapshot.kind === 'ollama') {
    return { kind: 'ollama', baseUrl: snapshot.baseUrl, model: snapshot.model };
  }
  return null;
}

export interface RoutingContext {
  taskClass: RoutingTaskClass;
  /** True when this turn has an image attached — a hard constraint, never a
   * policy preference. */
  requiresVision: boolean;
  /** True when this surface offers tools to the model. */
  requiresTools: boolean;
}

export interface RoutedTarget {
  /** The target to run. Equal to the `active` argument when no policy applied. */
  target: ResolvedTarget;
  decision: RoutingDecision;
  /** The policy's ordered attempt sequence, chosen target first. Empty when no
   * policy applied, so the caller keeps its existing failover behavior. */
  sequence: ResolvedTarget[];
}

/**
 * K9 dispatch policy: decides which configured model executes this piece of
 * work, starting from the target that would have run without any policy.
 *
 * Synchronous, and deliberately does **not** apply the choice to global model
 * state — the caller does that, because a chat turn's routed target should
 * stick (session affinity, via `applyTargetSwitch`) while a subagent's must
 * never move the model the user is chatting with.
 *
 * Ordering, which is the part that keeps a policy from widening anything:
 * every caller routes *before* its Privacy Firewall gate, so the routed
 * target is gated exactly like a hand-picked one and the firewall's own
 * switch-to-local still overrides a policy. Nothing here reads or writes a
 * permission mode, a workspace root, or an egress rule.
 */
export function routeFromActive(active: ResolvedTarget, context: RoutingContext): RoutedTarget {
  const policies = useRoutingPolicyStore.getState().policies;
  const activeKey = snapshotForResolvedTarget(active)?.key ?? null;
  const snapshots = new Map<string, ModelTargetSnapshot>();
  const candidates: RoutingCandidate[] = [];
  for (const snapshot of currentTargetInventory().targets) {
    if (resolvedFromSnapshot(snapshot) === null && snapshot.key !== activeKey) continue;
    snapshots.set(snapshot.key, snapshot);
    candidates.push(routingCandidate(snapshot));
  }

  const decision = routeRequest(policies, candidates, context, activeKey);
  useRoutingPolicyStore.getState().recordDecision(decision);

  const sequence: ResolvedTarget[] = [];
  for (const key of decision.sequence) {
    // The active target keeps its already-resolved form (it may be managed
    // llama.cpp, which has no snapshot conversion) rather than being rebuilt.
    if (key === activeKey) {
      sequence.push(active);
      continue;
    }
    const snapshot = snapshots.get(key);
    const resolved = snapshot ? resolvedFromSnapshot(snapshot) : null;
    if (resolved) sequence.push(resolved);
  }

  return { target: sequence[0] ?? active, decision, sequence };
}

/** `routeFromActive` starting from whatever target is currently selected —
 * the entry point for every surface that dispatches the user's active model.  */
export async function routeTarget(context: RoutingContext): Promise<RoutedTarget> {
  return routeFromActive(await resolveTarget(), context);
}
