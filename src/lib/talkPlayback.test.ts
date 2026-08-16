import { describe, expect, it } from 'vitest';

import { createTalkPlayer, type PlaybackAudio } from './talkPlayback';

/** An audio element that reports what it was asked to do with a clip. */
class FakeAudio implements PlaybackAudio {
  currentTime = 0;
  onended: ((...args: never[]) => unknown) | null = null;
  onerror: ((...args: never[]) => unknown) | null = null;
  sinks: string[] = [];
  plays = 0;
  pauses = 0;
  setSinkId?: (deviceId: string) => Promise<void>;

  constructor(
    readonly url: string,
    options: { routing?: 'supported' | 'absent' | 'refused'; refusesToPlay?: boolean } = {},
  ) {
    if (options.routing === 'supported') {
      this.setSinkId = async (deviceId) => {
        this.sinks.push(deviceId);
      };
    }
    if (options.routing === 'refused') {
      this.setSinkId = async (deviceId) => {
        this.sinks.push(deviceId);
        throw new Error('Requested device not found');
      };
    }
    this.refusesToPlay = options.refusesToPlay ?? false;
  }

  private refusesToPlay: boolean;

  async play(): Promise<void> {
    if (this.refusesToPlay) throw new Error('play() failed because the user did not interact');
    this.plays += 1;
  }

  pause(): void {
    this.pauses += 1;
  }

  end(): void {
    this.onended?.();
  }
}

/** A player wired to one fake element, with the object URLs it opened and closed. */
function harness(options: ConstructorParameters<typeof FakeAudio>[1] = {}) {
  const opened: string[] = [];
  const revoked: string[] = [];
  const elements: FakeAudio[] = [];
  const player = createTalkPlayer({
    createObjectUrl: () => {
      const url = `blob:clip-${opened.length + 1}`;
      opened.push(url);
      return url;
    },
    revokeObjectUrl: (url) => revoked.push(url),
    createAudio: (url) => {
      const audio = new FakeAudio(url, options);
      elements.push(audio);
      return audio;
    },
  });
  return { player, opened, revoked, elements };
}

const CLIP = new Blob(['audio'], { type: 'audio/wav' });

describe('createTalkPlayer', () => {
  it('plays a configured output through the sink it was given', async () => {
    const { player, elements, revoked } = harness({ routing: 'supported' });
    const playing = player.play(CLIP, 'speaker-2');
    await Promise.resolve();
    await Promise.resolve();
    expect(elements[0].sinks).toEqual(['speaker-2']);
    expect(elements[0].plays).toBe(1);

    elements[0].end();
    expect(await playing).toBe(true);
    expect(revoked).toEqual(['blob:clip-1']);
  });

  it('still plays when the browser cannot route to a chosen output at all', async () => {
    const { player, elements } = harness({ routing: 'absent' });
    const playing = player.play(CLIP, 'speaker-2');
    await Promise.resolve();
    await Promise.resolve();
    // No `setSinkId` means the system default, which is audible. Refusing to
    // play what cannot be routed would end the conversation over a preference.
    expect(elements[0].plays).toBe(1);
    elements[0].end();
    expect(await playing).toBe(true);
  });

  it('falls back to the default output when the device is refused', async () => {
    const { player, elements } = harness({ routing: 'refused' });
    const playing = player.play(CLIP, 'unplugged-headphones');
    await Promise.resolve();
    await Promise.resolve();
    expect(elements[0].sinks).toEqual(['unplugged-headphones']);
    expect(elements[0].plays).toBe(1);
    elements[0].end();
    expect(await playing).toBe(true);
  });

  it('leaves the system default alone when nothing is configured', async () => {
    const { player, elements } = harness({ routing: 'supported' });
    const playing = player.play(CLIP, null);
    await Promise.resolve();
    await Promise.resolve();
    expect(elements[0].sinks).toEqual([]);
    elements[0].end();
    expect(await playing).toBe(true);
  });

  it('settles and releases the clip when playback is stopped mid-sentence', async () => {
    const { player, elements, revoked } = harness({ routing: 'supported' });
    const playing = player.play(CLIP, 'speaker-2');
    await Promise.resolve();
    await Promise.resolve();

    // A paused element fires neither `ended` nor `error`, so nothing else would
    // ever resolve this — the queue behind it would stall and the object URL
    // would outlive the window.
    player.stop();
    expect(await playing).toBe(false);
    expect(elements[0].pauses).toBe(1);
    expect(revoked).toEqual(['blob:clip-1']);

    // Stopping again, with nothing playing, is not an error.
    player.stop();
  });

  it('does not stall the queue behind a speaker that refuses to play', async () => {
    const { player, revoked } = harness({ routing: 'supported', refusesToPlay: true });
    expect(await player.play(CLIP, 'speaker-2')).toBe(false);
    expect(revoked).toEqual(['blob:clip-1']);
  });
});
