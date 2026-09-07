import { describe, expect, it } from 'vitest';

import { realtimeVoiceClient } from './companionClient';
import { OpenAiRealtimeVoiceProvider } from './openAiRealtimeVoice';
import type { RealtimeVoiceEvent, RealtimeVoiceSession } from './realtimeVoice';
import { buildRealtimeToolSurface, executeRealtimeToolCall } from './realtimeVoiceToolBridge';
import { useSessionStore } from '../store/sessionStore';

const explicitlyEnabled = process.env.LITTLE_MONKEY_REALTIME_INTEGRATION === '1';
const runningInsideDesktop = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/**
 * Opt-in, interactive real-provider acceptance. The operator saves an OpenAI
 * key through Settings, opens a workspace/chat in a desktop-hosted test
 * runner, sets LITTLE_MONKEY_REALTIME_TEST_PATH to a harmless readable file in
 * that workspace, and speaks one short request during the capture window.
 * No credential or provider payload is printed.
 */
describe.skipIf(!explicitlyEnabled || !runningInsideDesktop)('OpenAI realtime live integration', () => {
  it('runs microphone → provider tool → normal host tool → spoken answer → interruption → clean close', async () => {
    const testPath = process.env.LITTLE_MONKEY_REALTIME_TEST_PATH;
    if (!testPath) throw new Error('Set LITTLE_MONKEY_REALTIME_TEST_PATH to a harmless file in the active workspace.');
    const status = await realtimeVoiceClient.status();
    expect(status.configured).toBe(true);
    const store = useSessionStore.getState();
    const chatSessionId = store.activeSessionId;
    if (!store.sessions.some((candidate) => candidate.id === chatSessionId)) {
      throw new Error('Open an ordinary Little Monkey chat session before running this test.');
    }
    const surface = await buildRealtimeToolSurface([]);
    const readFile = surface.tools.find((tool) => tool.function.name === 'read_file');
    if (!readFile) throw new Error('The active workspace does not offer read_file.');

    const observed = new Set<string>();
    let session!: RealtimeVoiceSession;
    let toolResultReturned = false;
    let pendingTool: Promise<void> | null = null;
    let resolveExchange!: () => void;
    const exchange = new Promise<void>((resolve) => { resolveExchange = resolve; });
    const expectedEvents = [
      'input_transcript', 'tool_call', 'tool_result', 'output_transcript_done',
      'output_audio_started', 'interrupted',
    ];
    const maybeComplete = () => {
      if (expectedEvents.every((name) => observed.has(name))) resolveExchange();
    };
    const onEvent = (event: RealtimeVoiceEvent) => {
      observed.add(event.type);
      if (event.type === 'tool_call') {
        pendingTool = executeRealtimeToolCall(event.call, {
          chatSessionId,
          realtimeSessionId: 'rv_live_integration',
          surface,
        }).then((result) => {
          session.sendToolResult(event.call.id, result);
          toolResultReturned = true;
          observed.add('tool_result');
          maybeComplete();
        });
      } else if (event.type === 'response_done' && pendingTool) {
        const current = pendingTool;
        pendingTool = null;
        void current.then(() => session.requestResponse());
      } else if (event.type === 'output_audio_started' && toolResultReturned) {
        void session.interrupt();
      }
      maybeComplete();
    };

    session = new OpenAiRealtimeVoiceProvider().createSession({
      sessionId: `rv_integration_${crypto.randomUUID()}`,
      model: 'gpt-realtime-2.1',
      voice: 'marin',
      turnDetection: 'manual',
      inputDeviceId: null,
      outputDeviceId: null,
      instructions: `This is an integration test. After the user speaks, you MUST call read_file exactly once with path ${JSON.stringify(testPath)}. After receiving its result, speak a short summary.`,
      tools: [readFile],
    }, onEvent);
    await session.connect();
    expect(session.state).toBe('ready');
    await session.startManualTurn();
    await new Promise((resolve) => window.setTimeout(resolve, 8_000));
    await session.finishManualTurn();
    await Promise.race([
      exchange,
      new Promise<never>((_, reject) => window.setTimeout(
        () => reject(new Error('Timed out waiting for the real spoken/tool exchange.')),
        60_000,
      )),
    ]);
    for (const expected of expectedEvents) expect(observed.has(expected)).toBe(true);
    await session.close();
    expect(session.state).toBe('closed');
  }, 90_000);
});
