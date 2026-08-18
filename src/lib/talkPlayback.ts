/**
 * The one place a synthesized clip becomes sound.
 *
 * Talk and the speaker test each built their own `new Audio(url)`, and only the
 * test applied `setSinkId` — so the output device the operator chose was true
 * of the test phrase and false of every conversation that followed it. There is
 * one path now, and the device is applied on it.
 *
 * `setSinkId` is the only way a page can choose an output and it is not
 * everywhere. Where it is missing, or where it refuses the device, the clip is
 * still played on the system default: a routing preference is not worth losing
 * the conversation over.
 */

/** The part of `HTMLAudioElement` playback uses, so a test can supply one. */
export interface PlaybackAudio {
  play(): Promise<void>;
  pause(): void;
  currentTime: number;
  onended: ((...args: never[]) => unknown) | null;
  onerror: ((...args: never[]) => unknown) | null;
  setSinkId?: (deviceId: string) => Promise<void>;
}

export interface TalkPlayerDeps {
  createAudio?: (url: string) => PlaybackAudio;
  createObjectUrl?: (blob: Blob) => string;
  revokeObjectUrl?: (url: string) => void;
}

export interface TalkPlayer {
  /**
   * Play one clip through `outputDeviceId`, or through the system default when
   * it is null. Resolves once the clip has finished, failed or been stopped;
   * `false` means nothing was heard.
   */
  play(blob: Blob, outputDeviceId: string | null): Promise<boolean>;
  /**
   * Stop what is playing. The pending `play` settles here rather than waiting
   * for an event that will not arrive: a paused element fires neither `ended`
   * nor `error`, so an interrupted chunk used to leave its promise unresolved
   * and its object URL alive for the life of the window.
   */
  stop(): void;
}

async function routeToOutput(audio: PlaybackAudio, outputDeviceId: string | null): Promise<void> {
  if (!outputDeviceId || typeof audio.setSinkId !== 'function') return;
  try {
    await audio.setSinkId(outputDeviceId);
  } catch {
    // The device was unplugged, or the browser refused it. The system default
    // is audible; a silent turn is not.
  }
}

export function createTalkPlayer(deps: TalkPlayerDeps = {}): TalkPlayer {
  const createAudio = deps.createAudio ?? ((url: string) => new Audio(url) as PlaybackAudio);
  const createObjectUrl = deps.createObjectUrl ?? ((blob: Blob) => URL.createObjectURL(blob));
  const revokeObjectUrl = deps.revokeObjectUrl ?? ((url: string) => URL.revokeObjectURL(url));
  let stopCurrent: (() => void) | null = null;

  return {
    play(blob, outputDeviceId) {
      const url = createObjectUrl(blob);
      const audio = createAudio(url);
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
        void routeToOutput(audio, outputDeviceId)
          .then(() => audio.play())
          .catch(() => finish(false));
      });
    },
    stop() {
      stopCurrent?.();
    },
  };
}
