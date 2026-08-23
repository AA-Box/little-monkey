import { invoke } from '@tauri-apps/api/core';
import type { ToolCall } from './llamaClient';
import { executeToolCall } from './turnEngine';
import { ComputerUseRunBudget } from './taskCoordinator';

type NativeTarget = {
  applicationId: string;
  applicationName: string;
  windowId: string;
  windowTitle: string;
};

type Session = { sessionId: string; allowedWindows: string[] };

type ComputerUseFailureLike = {
  code?: unknown;
  inputSent?: unknown;
  safeToRetry?: unknown;
  phase?: unknown;
};

const turnId = 'computer-use-full-product-golden';

function toolCall(name: string, args: Record<string, unknown>, index: number): ToolCall {
  return {
    id: `${turnId}-${index}`,
    type: 'function',
    function: { name, arguments: JSON.stringify(args) },
  };
}

function parseResult(result: string): any {
  const parsed = JSON.parse(result) as any;
  if (parsed && typeof parsed === 'object' && parsed.error) {
    throw new Error(typeof parsed.error === 'string' ? parsed.error : JSON.stringify(parsed.error));
  }
  return parsed;
}

function failureFromError(error: unknown): ComputerUseFailureLike | null {
  if (!(error instanceof Error)) return null;
  try {
    const parsed = JSON.parse(error.message) as unknown;
    return parsed && typeof parsed === 'object' ? parsed as ComputerUseFailureLike : null;
  } catch {
    return null;
  }
}

function isInputSentUnverified(error: unknown): boolean {
  const failure = failureFromError(error);
  return failure?.code === 'INPUT_SENT_UNVERIFIED'
    && failure.inputSent === true
    && failure.safeToRetry === false
    && failure.phase === 'verify';
}

/** Runs only when the CI harness explicitly enables the product golden. Every
 * model-facing operation below enters the real executeToolCall dispatcher;
 * its native calls therefore cross the running Tauri IPC boundary. */
export async function runComputerUseFullProductE2e(): Promise<void> {
  const trace = {
    status: 'failed',
    real_frontend_dispatcher: false,
    task_coordinator: false,
    real_tauri_ipc: false,
    desktop_control_state: 'unknown',
    native_provider: 'unknown',
    real_desktop_actions_executed: false,
    postconditions: { dark_mode: false, profile: '', saved: false },
    state_verified: false,
    screenshot_received_by_frontend: false,
    screenshot_artifact_id: '',
    screenshot_base64: '',
    unverified_actions_resolved_by_reobservation: [] as string[],
    model_loop: { kind: 'deterministic-frontend-model-tool-loop', completed: false, tool_calls: [] as string[] },
    tool_calls: [] as Array<{ name: string; result: string; durationMs: number }>,
    error: null as string | null,
  };
  let sessionId: string | undefined;
  const budget = new ComputerUseRunBudget();
  let callIndex = 0;
  const pid = Number(import.meta.env.VITE_COMPUTER_USE_FIXTURE_PID ?? '0');
  const allowedApplications = [
    `process:${pid}`,
    'Python', 'python', 'python3', 'python.exe',
    'atspi:Python', 'atspi:python', 'atspi:python3',
    'Little Monkey TestApp', 'com.aabox.LittleMonkeyTestApp',
  ];

  const dispatch = async (name: string, args: Record<string, unknown>): Promise<any> => {
    const startedAt = performance.now();
    const result = await executeToolCall(
      toolCall(name, args, callIndex),
      null,
      turnId,
      new Map(),
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      { computerUseBudget: budget },
    );
    callIndex += 1;
    trace.real_frontend_dispatcher = true;
    trace.task_coordinator = true;
    trace.real_tauri_ipc = true;
    trace.tool_calls.push({ name, result, durationMs: Math.round(performance.now() - startedAt) });
    trace.model_loop.tool_calls.push(name);
    return parseResult(result);
  };

  const start = async (allowedWindows?: string[]): Promise<Session> => invoke('desktop_control_start_session', {
    allowedApplications,
    lifetimeMs: 600_000,
    approvedBatch: true,
    allowedWindows: allowedWindows ?? [],
    allowScreenshots: true,
    allowKeyboardInput: true,
    allowClipboardRead: false,
    approvalPolicy: 'approved_batch',
  });

  try {
    if (!pid) throw new Error('full product fixture pid is missing');
    const discovery = await start();
    try {
      const discovered = (await dispatch('computer_list_targets', { session_id: discovery.sessionId })) as NativeTarget[];
      const target = discovered.find((entry) => entry.windowTitle === 'Little Monkey TestApp') ?? discovered.find((entry) => /little monkey|python/i.test(entry.applicationName));
      if (!target) throw new Error('full product dispatcher could not discover the fixture window');
      await invoke('desktop_control_stop_session', { sessionId: discovery.sessionId });

      const scoped = await start([target.windowId]);
      sessionId = scoped.sessionId;
      const common = {
        session_id: sessionId,
        target_application_id: target.applicationId,
        target_window_id: target.windowId,
      };
      await dispatch('computer_focus', common);
      let inspection = await dispatch('computer_inspect', common);
      const elements = () => (inspection.elements ?? []) as Array<Record<string, any>>;
      const actionsOf = (element: Record<string, any>) => Array.isArray(element.actions)
        ? element.actions.map((action: unknown) => String(action))
        : [];
      const dark = elements().find((element) => element.label === 'Dark mode'
        && /check/i.test(String(element.role))
        && actionsOf(element).includes('click'));
      const profile = elements().find((element) => element.label === 'Profile name'
        && /edit/i.test(String(element.role))
        && actionsOf(element).includes('set_value'));
      const save = elements().find((element) => element.label === 'Save profile'
        && /button/i.test(String(element.role))
        && actionsOf(element).includes('click'));
      if (!dark || !profile || !save) throw new Error('full product inspection did not expose semantic fixture controls');

      await dispatch('computer_click', {
        ...common,
        element_id: dark.id,
        button: 'left',
      });
      inspection = await dispatch('computer_inspect', common);
      const darkAfter = elements().find((element) => element.id === dark.id)
        ?? elements().find((element) => element.label === 'Dark mode' && /check/i.test(String(element.role)));
      trace.postconditions.dark_mode = /on|true|checked|togglestate\.on|1/i.test(String(darkAfter?.value ?? ''));

      const profileValue = `frontend-real-os-golden-${pid}`;
      let profileVerified = false;
      let setValueUnverified: unknown = null;
      try {
        const setResult = await dispatch('computer_set_value', {
          ...common,
          element_id: profile.id,
          value: profileValue,
        });
        profileVerified = Boolean(setResult.stateVerified);
      } catch (error) {
        if (!isInputSentUnverified(error)) throw error;
        // The mutation boundary has already been crossed. Never resend the
        // value. Resolve the typed uncertainty only by fresh observations.
        setValueUnverified = error;
      }

      const observedProfile = () => elements().find((element) => element.id === profile.id)
        ?? elements().find((element) => element.label === 'Profile name'
          && /edit/i.test(String(element.role))
          && actionsOf(element).includes('set_value'));
      for (let attempt = 0; !profileVerified && attempt < 10; attempt += 1) {
        if (attempt > 0) await new Promise((resolve) => setTimeout(resolve, 200));
        inspection = await dispatch('computer_inspect', common);
        profileVerified = String(observedProfile()?.value ?? '') === profileValue;
      }
      if (!profileVerified) {
        if (setValueUnverified) throw setValueUnverified;
        throw new Error('profile value was not re-observed after computer_set_value');
      }
      if (setValueUnverified) {
        trace.unverified_actions_resolved_by_reobservation.push('computer_set_value');
      }
      trace.postconditions.profile = String(observedProfile()?.value ?? '');

      let saveUnverified: unknown = null;
      try {
        await dispatch('computer_click', {
          ...common,
          element_id: save.id,
          button: 'left',
        });
      } catch (error) {
        if (!isInputSentUnverified(error)) throw error;
        // "Saved" lives on a different semantic element than the button.
        // Do not click again; prove that cross-element postcondition by
        // re-inspection instead.
        saveUnverified = error;
      }

      const savedObserved = () => elements().some((element) => String(element.label ?? '').trim().toLowerCase() === 'saved'
        || String(element.value ?? '').trim().toLowerCase() === 'saved');
      inspection = await dispatch('computer_inspect', common);
      trace.postconditions.saved = savedObserved();
      for (let attempt = 0; !trace.postconditions.saved && attempt < 9; attempt += 1) {
        await new Promise((resolve) => setTimeout(resolve, 200));
        inspection = await dispatch('computer_inspect', common);
        trace.postconditions.saved = savedObserved();
      }
      if (!trace.postconditions.saved) {
        if (saveUnverified) throw saveUnverified;
        throw new Error('Saved status was not re-observed after computer_click');
      }
      if (saveUnverified) {
        trace.unverified_actions_resolved_by_reobservation.push('computer_click:Save profile');
      }

      trace.state_verified = profileVerified && trace.postconditions.saved;
      const screenshot = await dispatch('computer_screenshot', common);
      trace.screenshot_received_by_frontend = typeof screenshot.contentBase64 === 'string' && screenshot.contentBase64.length > 100;
      trace.screenshot_artifact_id = String(screenshot.artifactId ?? '');
      trace.screenshot_base64 = typeof screenshot.contentBase64 === 'string' ? screenshot.contentBase64 : '';
      const provider = await invoke<{ backend: string; provider: string }>('desktop_control_provider_info');
      trace.desktop_control_state = provider.backend;
      trace.native_provider = provider.provider;
      trace.real_desktop_actions_executed = trace.tool_calls.some((call) => call.name === 'computer_set_value')
        && trace.postconditions.profile === profileValue
        && trace.postconditions.dark_mode;
      trace.status = trace.real_frontend_dispatcher
        && trace.task_coordinator
        && trace.real_tauri_ipc
        && trace.desktop_control_state === 'production'
        && trace.native_provider !== 'unsupported'
        && trace.real_desktop_actions_executed
        && trace.postconditions.saved
        && trace.screenshot_received_by_frontend
        && trace.state_verified
        ? 'completed'
        : 'failed';
      trace.model_loop.completed = trace.status === 'completed';
    } finally {
      if (discovery.sessionId !== sessionId) {
        await invoke('desktop_control_stop_session', { sessionId: discovery.sessionId }).catch(() => undefined);
      }
    }
  } catch (error) {
    trace.error = error instanceof Error ? error.message : String(error);
  } finally {
    if (sessionId) await invoke('desktop_control_stop_session', { sessionId }).catch(() => undefined);
    await invoke('computer_use_full_product_report', { report: trace });
  }
}
