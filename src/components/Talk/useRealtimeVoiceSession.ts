import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';

import { realtimeVoiceClient, type VoiceConfig } from '../../lib/companionClient';
import { beginDurableRun, type DurableRunRecorder } from '../../lib/durableRun';
import {
  type VoiceRouteRecord,
  voiceRouteActivate,
  voiceRouteDeactivate,
  voiceRouteEmit,
  voiceRouteEvents,
} from '../../lib/daemonClient';
import type { ProviderModelTargetSnapshot } from '../../lib/modelTargets';
import { OpenAiRealtimeVoiceProvider } from '../../lib/openAiRealtimeVoice';
import { RealtimeRouteBridge } from '../../lib/realtimeRouteBridge';
import {
  RealtimeVoiceController,
  boundedRealtimeContext,
  type RealtimeToolSettlement,
  type RealtimeVoiceEvent,
  type RealtimeVoiceSession,
  type RealtimeVoiceState,
} from '../../lib/realtimeVoice';
import {
  appendRealtimeTranscript,
  buildRealtimeToolSurface,
  executeRealtimeToolCall,
  type RealtimeToolSurface,
} from '../../lib/realtimeVoiceToolBridge';
import { usePermissionStore } from '../../store/permissionStore';
import { useSessionStore } from '../../store/sessionStore';
import { useSettingsStore } from '../../store/settingsStore';
import { useStackStore } from '../../store/stackStore';
import { useWorkspaceStore } from '../../store/workspaceStore';

function connectionFailureCode(message: string): string {
  const normalized = message.toLowerCase();
  if (normalized.includes('credential') || normalized.includes('api key') || normalized.includes('401') || normalized.includes('403')) {
    return 'credential_rejected';
  }
  if (normalized.includes('network') || normalized.includes('offline') || normalized.includes('connect')) {
    return 'network_unavailable';
  }
  return 'provider_negotiation_failed';
}

function conversationContext(chatSessionId: string): string {
  const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === chatSessionId);
  if (!session) return '';
  return boundedRealtimeContext(session.messages);
}

function realtimeTarget(model: string): ProviderModelTargetSnapshot {
  const target: ProviderModelTargetSnapshot = {
    kind: 'provider',
    key: `provider:openai:${model}`,
    label: 'OpenAI Realtime',
    displayName: model,
    providerId: 'openai',
    endpoint: 'https://api.openai.com/v1',
    model,
    credentialRefId: 'keychain:com.littlemonkey.app:openai',
    estimatedMemoryBytes: 0,
    capabilities: {
      toolCalling: { state: 'yes', evidence: 'OpenAI Realtime function calling.' },
      vision: { state: 'no', evidence: 'Desktop Talk sends audio and text only.' },
    },
    availability: { status: 'available', evidence: 'OpenAI key is configured in the OS keychain.' },
  };
  return Object.freeze(target);
}

async function stableRunId(realtimeSessionId: string, itemId: string): Promise<string> {
  const bytes = new TextEncoder().encode(`${realtimeSessionId}:${itemId}`);
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  const hex = [...new Uint8Array(digest)].slice(0, 16).map((byte) => byte.toString(16).padStart(2, '0')).join('');
  return `realtime-${hex}`;
}

function pairedInputDevice(value: string | undefined | null): string | null {
  return value?.match(/^paired:(.+):input$/)?.[1] ?? null;
}

function pairedOutputDevice(value: string | undefined | null): string | null {
  return value?.match(/^paired:(.+):output$/)?.[1] ?? null;
}

function localDevice(value: string | undefined | null, direction: 'input' | 'output'): string | null {
  const prefix = `local:${direction}:`;
  if (!value?.startsWith(prefix)) return null;
  const id = value.slice(prefix.length);
  return !id || id === 'default' ? null : id;
}

export interface UseRealtimeVoiceSession {
  state: RealtimeVoiceState;
  inputTranscript: string;
  outputTranscript: string;
  error: string | null;
  awaitingApproval: boolean;
  start: () => Promise<void>;
  stop: () => Promise<void>;
  interrupt: () => Promise<void>;
  startManualTurn: () => Promise<void>;
  finishManualTurn: () => Promise<void>;
}

export function useRealtimeVoiceSession(
  chatSessionId: string,
  voice: VoiceConfig,
  route: VoiceRouteRecord | null = null,
): UseRealtimeVoiceSession {
  const controller = useMemo(() => new RealtimeVoiceController(), []);
  const provider = useMemo(() => new OpenAiRealtimeVoiceProvider(), []);
  const [state, setState] = useState<RealtimeVoiceState>('idle');
  const [inputTranscript, setInputTranscript] = useState('');
  const [outputTranscript, setOutputTranscript] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [awaitingApproval, setAwaitingApproval] = useState(false);
  const awaitingApprovalRef = useRef(false);
  const sessionRef = useRef<RealtimeVoiceSession | null>(null);
  const realtimeSessionIdRef = useRef<string | null>(null);
  const surfaceRef = useRef<RealtimeToolSurface | null>(null);
  const recorderRef = useRef<DurableRunRecorder | null>(null);
  const recorderPromiseRef = useRef<Promise<DurableRunRecorder | null> | null>(null);
  const checkpointPromiseRef = useRef<Promise<string | null> | null>(null);
  const turnIdRef = useRef<string | null>(null);
  const outputByItemRef = useRef(new Map<string, string>());
  const reconnectAttemptedRef = useRef(false);
  /** Identity of the spoken turn in progress. It deliberately survives a
   * reconnect: the replacement provider session has new item ids, so this is
   * what stops an already-executed tool from running a second time. */
  const voiceTurnIdRef = useRef<string | null>(null);
  const toolTailRef = useRef<Promise<void>>(Promise.resolve());
  /** Cancels a tool still running when the operator ends Talk, so the durable
   * run is not cancelled out from under work that keeps going. */
  const toolAbortRef = useRef<AbortController | null>(null);
  /** True while no spoken turn is waiting to be finalized. Both the ordinary
   * `response.done` path and the tool tail can be the last thing to happen in
   * a turn, and the durable run must be closed by exactly one of them. */
  const turnFinalizedRef = useRef(true);
  const lastOutputTextRef = useRef('');
  const startingRef = useRef(false);
  const metricRecordedRef = useRef(false);
  const restartRef = useRef<() => Promise<void>>(async () => undefined);
  const routeRef = useRef<VoiceRouteRecord | null>(route);
  const routeCursorRef = useRef<{ generation: number; eventId: number } | null>(null);
  const bridgeRef = useRef<RealtimeRouteBridge | null>(null);
  const manualExternalInputRef = useRef(false);

  const recordMetric = useCallback(() => {
    if (metricRecordedRef.current) return;
    metricRecordedRef.current = true;
    const metric = controller.metrics;
    void realtimeVoiceClient.recordMetric({
      createdAtMs: Date.now(),
      connectionMs: metric.connectionMs,
      firstRecognizedSpeechMs: metric.firstRecognizedSpeechMs,
      firstModelEventMs: metric.firstModelEventMs,
      firstAudioMs: metric.firstAudioMs,
      endToEndMs: metric.endToEndMs,
      interrupted: metric.interrupted,
      reconnectCount: metric.reconnectCount,
      errorCode: metric.errorCode,
      toolRoundTripMs: metric.toolRoundTripMs,
      outputUnderruns: metric.outputUnderruns,
      inputTokens: metric.inputTokens,
      outputTokens: metric.outputTokens,
    }).catch(() => undefined);
  }, [controller]);

  const closeCurrent = useCallback(async () => {
    bridgeRef.current?.stop();
    bridgeRef.current = null;
    manualExternalInputRef.current = false;
    const current = sessionRef.current;
    sessionRef.current = null;
    await current?.close();
  }, []);

  const activateAndWaitForRoute = useCallback(async (selected: VoiceRouteRecord): Promise<VoiceRouteRecord> => {
    if (selected.engine !== 'realtime') throw new Error('This audio route belongs to Pipeline Talk, not Realtime.');
    const activated = await voiceRouteActivate(chatSessionId);
    routeRef.current = activated;
    const required = new Map<string, 'input_ready' | 'output_ready'>();
    const inputDevice = pairedInputDevice(activated.input_endpoint);
    const outputDevice = pairedOutputDevice(activated.output_endpoint);
    if (inputDevice && activated.input_command_id) required.set(activated.input_command_id, 'input_ready');
    if (outputDevice && outputDevice !== inputDevice && activated.output_command_id) {
      required.set(activated.output_command_id, 'output_ready');
    }
    if (required.size === 0) return activated;
    let cursor = routeCursorRef.current?.generation === activated.generation
      ? routeCursorRef.current.eventId
      : 0;
    const deadline = Date.now() + 20_000;
    while (required.size && Date.now() < deadline) {
      const events = await voiceRouteEvents(chatSessionId, cursor, 100);
      for (const event of events) {
        cursor = Math.max(cursor, event.event_id);
        if (event.generation !== activated.generation || !event.payload || typeof event.payload !== 'object') continue;
        const commandId = (event.payload as Record<string, unknown>).command_id;
        if (typeof commandId === 'string' && required.get(commandId) === event.kind) required.delete(commandId);
      }
      routeCursorRef.current = { generation: activated.generation, eventId: cursor };
      if (required.size) await new Promise((resolve) => setTimeout(resolve, 120));
    }
    if (required.size) throw new Error('A paired Realtime endpoint did not become ready within 20 seconds.');
    return activated;
  }, [chatSessionId]);

  const setRemoteInputGate = useCallback(async (open: boolean): Promise<void> => {
    const selected = routeRef.current;
    if (!selected || selected.state !== 'active' || !pairedInputDevice(selected.input_endpoint)) return;
    const requested = await voiceRouteEmit(chatSessionId, selected.generation, 'input_gate', { open });
    const deadline = Date.now() + 8_000;
    let cursor = requested.event_id;
    while (Date.now() < deadline) {
      const events = await voiceRouteEvents(chatSessionId, cursor, 100);
      for (const event of events) {
        cursor = Math.max(cursor, event.event_id);
        if (event.generation !== selected.generation || event.kind !== 'input_gate_ack'
            || !event.payload || typeof event.payload !== 'object') continue;
        const payload = event.payload as Record<string, unknown>;
        if (payload.gate_sequence === requested.event_id && payload.open === open) return;
      }
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    throw new Error(`Paired Realtime microphone did not ${open ? 'open' : 'close'} its input gate in time.`);
  }, [chatSessionId]);

  const configureRoute = useCallback(async (
    session: RealtimeVoiceSession,
    selected: VoiceRouteRecord | null,
  ) => {
    bridgeRef.current?.stop();
    bridgeRef.current = null;
    manualExternalInputRef.current = false;
    if (!selected || selected.state !== 'active') {
      await session.setInputRoute(false, voice.inputDeviceId);
      await session.setOutputRoute(false, voice.outputDeviceId);
      return;
    }
    const pairedInput = pairedInputDevice(selected.input_endpoint);
    const pairedOutput = pairedOutputDevice(selected.output_endpoint);
    const bridge = pairedInput || pairedOutput
      ? new RealtimeRouteBridge(chatSessionId, selected.generation, (reason) => {
          const message = reason.message || 'Realtime paired-device media bridge failed.';
          setError(message);
          controller.consume({
            type: 'connection_lost', eventId: `local:route-bridge:${crypto.randomUUID()}`,
            recoverable: false, code: 'paired_media_bridge_failed',
          });
          setState(controller.state);
          void closeCurrent();
          void voiceRouteDeactivate(chatSessionId).catch(() => null);
        })
      : null;
    bridgeRef.current = bridge;
    await session.setInputRoute(Boolean(pairedInput), pairedInput ? null : localDevice(selected.input_endpoint, 'input'));
    await session.setOutputRoute(
      Boolean(pairedOutput),
      pairedOutput ? null : localDevice(selected.output_endpoint, 'output'),
      pairedOutput && bridge ? (samples, sampleRate) => bridge.pushOutput(samples, sampleRate) : null,
    );
    if (pairedInput && bridge) {
      await bridge.startInput(
        (audioBase64) => session.appendInputPcm16(audioBase64),
        () => (voice.realtimeTurnDetection ?? 'semantic_vad') !== 'manual' || manualExternalInputRef.current,
      );
      if ((voice.realtimeTurnDetection ?? 'semantic_vad') === 'manual') {
        manualExternalInputRef.current = false;
        await setRemoteInputGate(false);
      }
    }
  }, [chatSessionId, closeCurrent, controller, setRemoteInputGate, voice.inputDeviceId, voice.outputDeviceId, voice.realtimeTurnDetection]);

  const startRecorder = useCallback(async (
    itemId: string,
    text: string,
    realtimeSessionId: string,
    anchorIndex: number,
  ) => {
    checkpointPromiseRef.current = invoke<string>('checkpoint_begin', {
      sessionId: chatSessionId,
      anchorIndex,
      label: text.slice(0, 120),
      maxKeep: useSettingsStore.getState().checkpointRetention,
    }).catch(() => null);
    const runId = await stableRunId(realtimeSessionId, itemId);
    turnIdRef.current = runId;
    const settings = useSettingsStore.getState();
    const promise = beginDurableRun({
      runId,
      idempotencyKey: `realtime/${realtimeSessionId}/${itemId}`,
      kind: 'interactive',
      task: text,
      instructions: `Realtime voice session for chat ${chatSessionId}`,
      target: realtimeTarget(voice.realtimeModel ?? 'gpt-realtime-2.1'),
      roots: useWorkspaceStore.getState().roots,
      permissionMode: usePermissionStore.getState().mode,
      allowNetwork: settings.webToolsEnabled,
      allowExternalMutations: usePermissionStore.getState().mode !== 'plan',
    }).catch(() => null);
    recorderPromiseRef.current = promise;
    recorderRef.current = await promise;
  }, [chatSessionId, voice.realtimeModel]);

  /** Closes out the spoken turn: ends its checkpoint, completes its durable
   * run, and releases the turn identity so a later identical tool call runs
   * normally. `awaitTools` is false when the caller is itself inside the tool
   * tail, which cannot await its own chain. */
  const finalizeTurn = useCallback((
    recorderPromise: Promise<DurableRunRecorder | null> | null,
    checkpointPromise: Promise<string | null> | null,
    awaitTools = true,
  ) => {
    if (turnFinalizedRef.current) return;
    turnFinalizedRef.current = true;
    const voiceTurnId = voiceTurnIdRef.current;
    const toolTail = awaitTools ? toolTailRef.current : null;
    void (async () => {
      // A cancelled response can finish while a tool is still running. The
      // durable run must still record that result before it is closed.
      await toolTail?.catch(() => undefined);
      const recorder = recorderRef.current ?? await recorderPromise;
      const checkpointId = await checkpointPromise;
      if (checkpointId) {
        const summary = await invoke<{ id: string; label?: string }>('checkpoint_end', { id: checkpointId }).catch(() => null);
        if (summary) recorder?.recordCheckpoint(summary.id, summary.label ?? 'Realtime voice turn');
      }
      await recorder?.complete(lastOutputTextRef.current || null);
      // Compared against the turn identity rather than the recorder promise:
      // a turn created lazily from a tool call has no recorder, and `null ===
      // null` would let it clear the next turn's bookkeeping.
      if (voiceTurnIdRef.current === voiceTurnId) {
        recorderRef.current = null;
        recorderPromiseRef.current = null;
        checkpointPromiseRef.current = null;
        turnIdRef.current = null;
        voiceTurnIdRef.current = null;
      }
    })();
  }, []);

  const handleEvent = useCallback((event: RealtimeVoiceEvent) => {
    if (!controller.consume(event)) return;
    setState(controller.state);
    const realtimeSessionId = realtimeSessionIdRef.current;
    if (!realtimeSessionId) return;
    if (event.type === 'input_transcript') {
      setInputTranscript(event.text);
      const anchorIndex = useSessionStore.getState().sessions.find((candidate) => candidate.id === chatSessionId)?.messages.length ?? 0;
      // A finalized user transcript starts a new spoken turn, so it also
      // starts a new host-side turn identity. Everything the turn executes is
      // keyed to it, including after a reconnect.
      const voiceTurnId = `vt_${crypto.randomUUID()}`;
      if (appendRealtimeTranscript(chatSessionId, realtimeSessionId, event.itemId, event.eventId, 'user', event.text, voiceTurnId)) {
        voiceTurnIdRef.current = voiceTurnId;
        turnFinalizedRef.current = false;
        lastOutputTextRef.current = '';
        void startRecorder(event.itemId, event.text, realtimeSessionId, anchorIndex);
      }
      return;
    }
    if (event.type === 'output_transcript_delta') {
      const next = `${outputByItemRef.current.get(event.itemId) ?? ''}${event.delta}`;
      outputByItemRef.current.set(event.itemId, next);
      lastOutputTextRef.current = next;
      setOutputTranscript(next);
      void (async () => {
        const recorder = recorderRef.current ?? await recorderPromiseRef.current;
        recorder?.recordModelOutput(event.itemId, event.delta);
      })();
      return;
    }
    if (event.type === 'output_transcript_done') {
      const text = event.text || outputByItemRef.current.get(event.itemId) || '';
      outputByItemRef.current.set(event.itemId, text);
      lastOutputTextRef.current = text;
      setOutputTranscript(text);
      appendRealtimeTranscript(
        chatSessionId, realtimeSessionId, event.itemId, event.eventId, 'assistant', text,
        voiceTurnIdRef.current ?? undefined,
      );
      return;
    }
    if (event.type === 'tool_call') {
      // A tool can be requested before any transcript was finalized (a barge-in
      // mid-turn, or a reconnect that resumes straight into a tool). The turn
      // still needs an identity for dedupe, so create one lazily.
      if (voiceTurnIdRef.current === null) {
        voiceTurnIdRef.current = `vt_${crypto.randomUUID()}`;
        turnFinalizedRef.current = false;
      }
      // Captured now, not read inside the continuation: `toolTailRef` serializes
      // executions, so a tool queued behind a slow one can otherwise run after
      // the next turn has replaced these refs and be recorded against it.
      const voiceTurnId = voiceTurnIdRef.current;
      const turnId = turnIdRef.current ?? undefined;
      const recorderPromise = recorderPromiseRef.current;
      const checkpointPromise = checkpointPromiseRef.current;
      const signal = toolAbortRef.current?.signal;
      toolTailRef.current = toolTailRef.current.catch(() => undefined).then(async () => {
        const toolStartedAt = performance.now();
        const current = sessionRef.current;
        let settlement: RealtimeToolSettlement = 'wait';
        try {
          const recorder = recorderRef.current ?? await recorderPromise;
          const checkpointId = await checkpointPromise;
          const surface = surfaceRef.current;
          if (!surface || !current) {
            // Nothing can answer this call any more, but the ledger must not be
            // left waiting on it or the turn would never finalize.
            settlement = controller.settleToolCall(event.call.id) === 'wait' ? 'wait' : 'closed';
            return;
          }
          let result: string;
          try {
            result = await executeRealtimeToolCall(event.call, {
              chatSessionId,
              realtimeSessionId,
              surface,
              recorder,
              checkpointId,
              turnId,
              voiceTurnId,
              signal,
              onAwaitingApproval: (waiting) => {
                awaitingApprovalRef.current = waiting;
                setAwaitingApproval(waiting);
                if (waiting) controller.awaitingApproval();
                setState(controller.state);
              },
            });
          } catch (reason) {
            result = JSON.stringify({ error: reason instanceof Error ? reason.message : String(reason) });
          }
          controller.recordToolRoundTrip(performance.now() - toolStartedAt);
          // The provider only speaks again when it is asked to. Whether this
          // result settles before or after `response.done`, exactly one of the
          // two paths asks — the ledger decides which, so a fast tool can
          // never leave the answer unspoken. A send that throws (the data
          // channel closed under us) must still settle the ledger, or the turn
          // would wait on this call forever.
          try {
            if (sessionRef.current === current) current.sendToolResult(event.call.id, result);
            settlement = controller.settleToolCall(event.call.id);
            if (settlement === 'continue' && sessionRef.current === current) current.requestResponse();
          } catch (reason) {
            // The data channel closed under us. The ledger must still settle or
            // the turn waits on this call forever, and the failure has to be
            // visible rather than looking like a model that went quiet.
            controller.settleToolCall(event.call.id);
            settlement = 'closed';
            const message = 'The realtime connection dropped before the tool result could be delivered.';
            setError(message);
            controller.consume({
              type: 'error', eventId: `local:tool-delivery:${crypto.randomUUID()}`,
              code: reason instanceof Error && reason.message.includes('data channel')
                ? 'data_channel_closed'
                : 'tool_result_undeliverable',
              message,
            });
            setState(controller.state);
            recordMetric();
            void closeCurrent();
          }
        } finally {
          awaitingApprovalRef.current = false;
          setAwaitingApproval(false);
          // Only a response that will never speak again ends the turn here.
          // A `continue` means another response is on its way and finalizing
          // now would clear the turn identity the reconnect protection needs.
          if (settlement === 'closed' && !controller.hasOutstandingToolCalls()) {
            finalizeTurn(recorderPromise, checkpointPromise, false);
          }
        }
      });
      return;
    }
    if (event.type === 'response_done') {
      const disposition = controller.responseDisposition(event.responseId);
      if (disposition === 'continue') {
        const current = sessionRef.current;
        current?.requestResponse();
        return;
      }
      // Its function calls are still running; the last one to settle asks for
      // the spoken follow-up and the turn is finalized by that response.
      if (disposition === 'await_tools') return;
      finalizeTurn(recorderPromiseRef.current, checkpointPromiseRef.current);
      return;
    }
    if (event.type === 'error') {
      setError(event.message);
      void recorderRef.current?.fail(event.message, false);
      recordMetric();
      void closeCurrent();
      return;
    }
    if (event.type === 'connection_lost') {
      // Only a permission prompt the operator is actually looking at blocks the
      // reconnect. A tool merely executing must not: a drop during a long tool
      // is the exact window the durable call identity was built to survive, and
      // treating it as unrecoverable throws the turn away instead.
      const decisionPending = usePermissionStore.getState().pending !== null;
      if (event.recoverable && !decisionPending && !reconnectAttemptedRef.current) {
        // The durable run is not failed here: the same turn continues in the
        // replacement session, and a terminal recorder would drop the tool and
        // output events that reconnect is meant to preserve.
        reconnectAttemptedRef.current = true;
        setError('The realtime connection was lost. Reconnecting once to the same provider…');
        void closeCurrent().then(() => restartRef.current());
      } else {
        void recorderRef.current?.fail(event.code, event.recoverable);
        const message = event.code === 'microphone_revoked'
          ? 'Microphone access ended. Check the selected device and system permission.'
          : 'The realtime connection was lost. Start again to retry the same provider.';
        setError(message);
        void closeCurrent();
        controller.consume({
          type: 'error', eventId: `local:connection-final:${crypto.randomUUID()}`,
          code: event.code, message,
        });
        setState(controller.state);
        recordMetric();
      }
    }
  }, [chatSessionId, closeCurrent, controller, finalizeTurn, recordMetric, startRecorder]);

  const start = useCallback(async () => {
    if (startingRef.current || sessionRef.current) return;
    startingRef.current = true;
    if (controller.state === 'idle' || controller.state === 'closed' || controller.state === 'error') {
      controller.reset();
      reconnectAttemptedRef.current = false;
      metricRecordedRef.current = false;
    }
    setError(null);
    setInputTranscript('');
    setOutputTranscript('');
    awaitingApprovalRef.current = false;
    setAwaitingApproval(false);
    lastOutputTextRef.current = '';
    // A reconnect keeps the turn identity it already has. A cold start adopts
    // the identity of a turn the durable transcript shows as unanswered, so a
    // restart cannot re-run what a previous process already executed.
    toolAbortRef.current ??= new AbortController();
    const realtimeSessionId = `rv_${crypto.randomUUID()}`;
    realtimeSessionIdRef.current = realtimeSessionId;
    try {
      const chatSession = useSessionStore.getState().sessions.find((candidate) => candidate.id === chatSessionId);
      const stackNames = useStackStore.getState().stacks
        .filter((stack) => chatSession?.attachedStackIds.includes(stack.id))
        .map((stack) => stack.name);
      const surface = await buildRealtimeToolSurface(stackNames);
      surfaceRef.current = surface;
      const context = conversationContext(chatSessionId);
      const instructions = [
        'You are Little Monkey in a live voice conversation. Be concise and natural.',
        'Use the supplied tools when needed. Tool results are authoritative; do not claim an action succeeded before receiving one.',
        context ? `Bounded read-only conversation context:\n${context}` : '',
      ].filter(Boolean).join('\n\n');
      controller.connecting(reconnectAttemptedRef.current);
      setState(controller.state);
      const selectedRoute = routeRef.current?.state === 'active' ? routeRef.current : null;
      if (selectedRoute && selectedRoute.engine !== 'realtime') {
        throw new Error('The selected audio route belongs to Pipeline Talk, not Realtime.');
      }
      const pairedInput = pairedInputDevice(selectedRoute?.input_endpoint);
      const pairedOutput = pairedOutputDevice(selectedRoute?.output_endpoint);
      const session = provider.createSession({
        sessionId: realtimeSessionId,
        model: voice.realtimeModel ?? 'gpt-realtime-2.1',
        voice: voice.realtimeVoice ?? 'marin',
        turnDetection: voice.realtimeTurnDetection ?? 'semantic_vad',
        inputDeviceId: selectedRoute ? localDevice(selectedRoute.input_endpoint, 'input') : voice.inputDeviceId,
        outputDeviceId: selectedRoute ? localDevice(selectedRoute.output_endpoint, 'output') : voice.outputDeviceId,
        externalInput: Boolean(pairedInput),
        externalOutput: Boolean(pairedOutput),
        instructions,
        tools: surface.tools,
      }, handleEvent);
      sessionRef.current = session;
      await session.connect();
      if (selectedRoute) {
        const activated = await activateAndWaitForRoute(selectedRoute);
        await configureRoute(session, activated);
      }
    } catch (reason) {
      await closeCurrent();
      const message = reason instanceof Error ? reason.message : String(reason);
      controller.consume({
        type: 'error', eventId: `local:start:${crypto.randomUUID()}`,
        code: connectionFailureCode(message), message,
      });
      setState(controller.state);
      setError(message);
      recordMetric();
    } finally {
      startingRef.current = false;
    }
  }, [activateAndWaitForRoute, chatSessionId, closeCurrent, configureRoute, controller, handleEvent, provider, recordMetric, voice]);

  useEffect(() => { restartRef.current = start; }, [start]);

  const stop = useCallback(async () => {
    // Ending Talk cancels the tool too. Without this the durable run is
    // cancelled while the work it describes keeps running to completion.
    toolAbortRef.current?.abort();
    toolAbortRef.current = null;
    await closeCurrent();
    await voiceRouteDeactivate(chatSessionId).catch(() => null);
    const recorder = recorderRef.current ?? await recorderPromiseRef.current;
    const checkpointId = await checkpointPromiseRef.current;
    if (checkpointId) await invoke('checkpoint_end', { id: checkpointId }).catch(() => undefined);
    if (recorder) await recorder.cancel('Realtime voice session ended.').catch(() => undefined);
    recorderRef.current = null;
    recorderPromiseRef.current = null;
    checkpointPromiseRef.current = null;
    turnIdRef.current = null;
    voiceTurnIdRef.current = null;
    turnFinalizedRef.current = true;
    awaitingApprovalRef.current = false;
    setAwaitingApproval(false);
    recordMetric();
    controller.close();
    setState('closed');
    reconnectAttemptedRef.current = false;
  }, [chatSessionId, closeCurrent, controller, recordMetric]);

  useEffect(() => () => {
    void closeCurrent();
    void voiceRouteDeactivate(chatSessionId).catch(() => null);
  }, [chatSessionId, closeCurrent]);

  useEffect(() => {
    routeRef.current = route;
    const current = sessionRef.current;
    if (!current) return;
    void (async () => {
      try {
        if (route?.state === 'active') {
          const activated = await activateAndWaitForRoute(route);
          if (sessionRef.current === current) await configureRoute(current, activated);
        } else if (sessionRef.current === current) {
          await configureRoute(current, null);
        }
      } catch (reason) {
        if (sessionRef.current !== current) return;
        const message = reason instanceof Error ? reason.message : String(reason);
        setError(message);
        controller.consume({
          type: 'connection_lost', eventId: `local:route-change:${crypto.randomUUID()}`,
          recoverable: false, code: 'voice_route_change_failed',
        });
        setState(controller.state);
        await closeCurrent();
      }
    })();
  }, [activateAndWaitForRoute, closeCurrent, configureRoute, controller, route]);

  useEffect(() => {
    const selected = route;
    if (!selected || selected.state !== 'active' || selected.engine !== 'realtime' || !sessionRef.current) return undefined;
    let stopped = false;
    let cursor = routeCursorRef.current?.generation === selected.generation
      ? routeCursorRef.current.eventId
      : 0;
    const poll = async () => {
      while (!stopped && sessionRef.current) {
        try {
          const events = await voiceRouteEvents(chatSessionId, cursor, 100);
          for (const event of events) {
            cursor = Math.max(cursor, event.event_id);
            if (event.generation !== selected.generation) continue;
            if (event.kind === 'interrupt') {
              await sessionRef.current?.interrupt();
              if (pairedOutputDevice(selected.output_endpoint)) {
                await bridgeRef.current?.interruptOutput().catch(() => undefined);
                await voiceRouteEmit(chatSessionId, selected.generation, 'output_stop', {}).catch(() => null);
              }
            }
          }
          routeCursorRef.current = { generation: selected.generation, eventId: cursor };
          await new Promise((resolve) => setTimeout(resolve, 120));
        } catch {
          if (!stopped) await new Promise((resolve) => setTimeout(resolve, 250));
        }
      }
    };
    void poll();
    return () => { stopped = true; };
  }, [chatSessionId, route]);

  useEffect(() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return undefined;
    const onDeviceChange = async () => {
      if (!sessionRef.current) return;
      const routedInput = routeRef.current?.state === 'active'
        ? localDevice(routeRef.current.input_endpoint, 'input')
        : voice.inputDeviceId;
      if (!routedInput) return;
      const devices = await mediaDevices.enumerateDevices().catch(() => []);
      if (!devices.some((device) => device.kind === 'audioinput' && device.deviceId === routedInput)) {
        setError('The selected microphone was removed. Reconnect it or choose another device.');
        await closeCurrent();
        controller.consume({
          type: 'connection_lost', eventId: `local:device-removed:${crypto.randomUUID()}`,
          recoverable: false, code: 'input_device_removed',
        });
        setState(controller.state);
        recordMetric();
      }
    };
    const onPageHide = () => { void closeCurrent(); };
    const onVisibilityChange = () => {
      if (!document.hidden || !sessionRef.current) return;
      const message = 'Realtime Talk ended because the app was suspended or hidden.';
      setError(message);
      controller.consume({
        type: 'error', eventId: `local:suspended:${crypto.randomUUID()}`,
        code: 'app_suspended', message,
      });
      setState(controller.state);
      recordMetric();
      void closeCurrent();
    };
    mediaDevices.addEventListener?.('devicechange', onDeviceChange);
    window.addEventListener('pagehide', onPageHide);
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => {
      mediaDevices.removeEventListener?.('devicechange', onDeviceChange);
      window.removeEventListener('pagehide', onPageHide);
      document.removeEventListener('visibilitychange', onVisibilityChange);
    };
  }, [closeCurrent, controller, recordMetric, voice.inputDeviceId]);

  const interrupt = useCallback(async () => {
    await sessionRef.current?.interrupt();
    const currentRoute = routeRef.current;
    if (currentRoute?.state === 'active' && pairedOutputDevice(currentRoute.output_endpoint)) {
      await bridgeRef.current?.interruptOutput().catch(() => undefined);
      await voiceRouteEmit(chatSessionId, currentRoute.generation, 'output_stop', {}).catch(() => null);
    }
  }, [chatSessionId]);

  const startManualTurn = useCallback(async () => {
    await sessionRef.current?.startManualTurn();
    if (pairedInputDevice(routeRef.current?.input_endpoint)) {
      manualExternalInputRef.current = true;
      try {
        await setRemoteInputGate(true);
      } catch (reason) {
        manualExternalInputRef.current = false;
        throw reason;
      }
    }
  }, [setRemoteInputGate]);

  const finishManualTurn = useCallback(async () => {
    if (pairedInputDevice(routeRef.current?.input_endpoint)) {
      await setRemoteInputGate(false);
      await bridgeRef.current?.waitForInputIdle();
      manualExternalInputRef.current = false;
    }
    await sessionRef.current?.finishManualTurn();
  }, [setRemoteInputGate]);

  return {
    state,
    inputTranscript,
    outputTranscript,
    error,
    awaitingApproval,
    start,
    stop,
    interrupt,
    startManualTurn,
    finishManualTurn,
  };
}
