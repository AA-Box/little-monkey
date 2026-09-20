/**
 * The one place a synthesized clip becomes sound.
 *
 * Talk and the speaker test each built their own `new Audio(url)`, and only the
 * test applied `setSinkId` — so the output device the operator chose was true
 * of the test phrase and false of every conversation that followed it. There is
 * one path now, and the device is applied on it.
 *
 * **One element, kept.** WebKit gates `setSinkId` on a user gesture, and an
 * answer arrives long after the press that asked for it — so a fresh
 * `new Audio()` per clip could never be routed: every call threw
 * `NotAllowedError: A user gesture is required`, the catch swallowed it, and
 * Talk played on the system default no matter what the picker said. The
 * element is retained and its `src` swapped per clip instead, because a sink
 * set inside a gesture survives that swap. `setOutput` is the gesture-time
 * call; `play` never touches the sink.
 *
 * Where `setSinkId` is missing, or the device is refused, the clip is still
 * played on the system default: a routing preference is not worth losing the
 * conversation over. `setOutput` returns whether it took, so a caller can say
 * so rather than leaving the picker claiming something untrue.
 */

/** The part of `HTMLAudioElement` playback uses, so a test can supply one. */
export interface PlaybackAudio {
  play(): Promise<void>;
  pause(): void;
  currentTime: number;
  src: string;
  onended: ((...args: never[]) => unknown) | null;
  onerror: ((...args: never[]) => unknown) | null;
  setSinkId?: (deviceId: string) => Promise<void>;
}

export interface TalkPlayerDeps {
  createAudio?: () => PlaybackAudio;
  createObjectUrl?: (blob: Blob) => string;
  revokeObjectUrl?: (url: string) => void;
}

export interface TalkPlayer {
  /**
   * Point playback at `deviceId`, or back at the system default when it is
   * null. Returns whether the browser accepted it.
   *
   * **Call this synchronously from a gesture handler** — the change event of a
   * speaker picker, or the press that opens the microphone. WebKit refuses
   * `setSinkId` outside a user gesture, and one `await` before it is enough to
   * spend one.
   */
  setOutput(deviceId: string | null): Promise<boolean>;
  /**
   * Play one clip through whatever `setOutput` last chose. Resolves once the
   * clip has finished, failed or been stopped; `false` means nothing was heard.
   */
  play(blob: Blob): Promise<boolean>;
  /**
   * Stop what is playing. The pending `play` settles here rather than waiting
   * for an event that will not arrive: a paused element fires neither `ended`
   * nor `error`, so an interrupted chunk used to leave its promise unresolved
   * and its object URL alive for the life of the window.
   */
  stop(): void;
  /**
   * Drop the element, and with it the chosen sink.
   *
   * Called when Talk closes its devices. The sink is a per-session choice —
   * `setOutput` runs inside the gesture that opens the microphone — so keeping
   * a stale one alive across sessions would route the next conversation
   * somewhere nobody picked for it.
   */
  release(): void;
}

export function createTalkPlayer(deps: TalkPlayerDeps = {}): TalkPlayer {
  const createAudio = deps.createAudio ?? (() => new Audio() as PlaybackAudio);
  const createObjectUrl = deps.createObjectUrl ?? ((blob: Blob) => URL.createObjectURL(blob));
  const revokeObjectUrl = deps.revokeObjectUrl ?? ((url: string) => URL.revokeObjectURL(url));
  let stopCurrent: (() => void) | null = null;
  let element: PlaybackAudio | null = null;

  /** The one element every clip plays through, so the sink outlives the clip. */
  const audioElement = (): PlaybackAudio => (element ??= createAudio());

  return {
    async setOutput(deviceId) {
      const audio = audioElement();
      if (typeof audio.setSinkId !== 'function') return false;
      try {
        // `''` is what the spec uses for "the user agent default", and it is
        // what a null choice must send: passing a stale id instead would keep
        // routing somewhere the operator has stopped asking for.
        await audio.setSinkId(deviceId ?? '');
        return true;
      } catch {
        // Refused, or the device is gone. The system default is audible; a
        // silent turn is not.
        return false;
      }
    },
    play(blob) {
      const url = createObjectUrl(blob);
      const audio = audioElement();
      audio.src = url;
      return new Promise<boolean>((resolve) => {
        let settled = false;
        const finish = (played: boolean) => {
          if (settled) return;
          settled = true;
          if (stopCurrent === stop) stopCurrent = null;
          revokeObjectUrl(url);
          resolve(played);
        };
        const stop = () => {
          audio.pause();
          audio.currentTime = 0;
          finish(false);
        };
        stopCurrent = stop;
        audio.onended = () => finish(true);
        audio.onerror = () => finish(false);
        // No `setSinkId` here on purpose: it would run outside a gesture and
        // throw, and the sink chosen at gesture time is already on this
        // element.
        void audio.play().catch(() => finish(false));
      });
    },
    stop() {
      stopCurrent?.();
    },
    release() {
      stopCurrent?.();
      element = null;
    },
  };
}

/**
 * The window's player.
 *
 * One element, because a sink belongs to an element: a second one would be a
 * second output, silently on the system default, which is the bug the file
 * header describes.
 */
export const talkPlayer = createTalkPlayer();
