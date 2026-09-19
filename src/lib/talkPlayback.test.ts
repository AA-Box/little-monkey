import { describe, expect, it } from 'vitest';

import { createTalkPlayer, type PlaybackAudio } from './talkPlayback';

/** An audio element that reports what it was asked to do with a clip. */
class FakeAudio implements PlaybackAudio {
  currentTime = 0;
  src = '';
  srcs: string[] = [];
  onended: ((...args: never[]) => unknown) | null = null;
  onerror: ((...args: never[]) => unknown) | null = null;
  sinks: string[] = [];
  plays = 0;
  pauses = 0;
  setSinkId?: (deviceId: string) => Promise<void>;

  constructor(
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
    this.srcs.push(this.src);
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
function harness(options: ConstructorParameters<typeof FakeAudio>[0] = {}) {
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
    createAudio: () => {
      const audio = new FakeAudio(options);
      elements.push(audio);
      return audio;
    },
  });
  return { player, opened, revoked, elements };
}

const CLIP = new Blob(['audio'], { type: 'audio/wav' });

describe('createTalkPlayer', () => {
  /**
   * The regression that made the speaker picker decorative.
   *
   * WebKit refuses `setSinkId` outside a user gesture, and an answer arrives
   * long after the press that asked for it. Routing at play time therefore
   * threw on every single turn, the catch swallowed it, and Talk played on the
   * system default no matter what was chosen. The sink is set once, at gesture
   * time, on an element that is kept.
   */
  it('never touches the sink while playing a clip', async () => {
    const { player, elements } = harness({ routing: 'supported' });
    await player.setOutput('speaker-2');
    expect(elements[0].sinks).toEqual(['speaker-2']);

    const playing = player.play(CLIP);
    await Promise.resolve();
    await Promise.resolve();
    // Still one: playback asked for no routing of its own.
    expect(elements[0].sinks).toEqual(['speaker-2']);
    elements[0].end();
    expect(await playing).toBe(true);
  });

  it('keeps one element, so the chosen sink outlives the clip', async () => {
    const { player, elements, revoked } = harness({ routing: 'supported' });
    await player.setOutput('speaker-2');

    const first = player.play(CLIP);
    await Promise.resolve();
    await Promise.resolve();
    elements[0].end();
    await first;

    const second = player.play(CLIP);
    await Promise.resolve();
    await Promise.resolve();
    elements[0].end();
    await second;

    // A second element would be a second output, on the system default.
    expect(elements).toHaveLength(1);
    expect(elements[0].srcs).toEqual(['blob:clip-1', 'blob:clip-2']);
    expect(elements[0].sinks).toEqual(['speaker-2']);
    expect(revoked).toEqual(['blob:clip-1', 'blob:clip-2']);
  });

  it('asks for the user agent default when the choice is cleared', async () => {
    const { player, elements } = harness({ routing: 'supported' });
    await player.setOutput('speaker-2');
    await player.setOutput(null);
    // Not "leave it alone": an empty id is the spec's way of saying default,
    // and anything else would keep routing where nobody asked any more.
    expect(elements[0].sinks).toEqual(['speaker-2', '']);
  });

  it('reports whether the browser took the device, rather than pretending', async () => {
    const supported = harness({ routing: 'supported' });
    expect(await supported.player.setOutput('speaker-2')).toBe(true);

    const absent = harness({ routing: 'absent' });
    expect(await absent.player.setOutput('speaker-2')).toBe(false);

    const refused = harness({ routing: 'refused' });
    expect(await refused.player.setOutput('unplugged-headphones')).toBe(false);
  });

  it('still plays when the browser cannot route to a chosen output at all', async () => {
    const { player, elements } = harness({ routing: 'absent' });
    await player.setOutput('speaker-2');
    const playing = player.play(CLIP);
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
    await player.setOutput('unplugged-headphones');
    expect(elements[0].sinks).toEqual(['unplugged-headphones']);
    const playing = player.play(CLIP);
    await Promise.resolve();
    await Promise.resolve();
    expect(elements[0].plays).toBe(1);
    elements[0].end();
    expect(await playing).toBe(true);
  });

  it('settles and releases the clip when playback is stopped mid-sentence', async () => {
    const { player, elements, revoked } = harness({ routing: 'supported' });
    const playing = player.play(CLIP);
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
    expect(await player.play(CLIP)).toBe(false);
    expect(revoked).toEqual(['blob:clip-1']);
  });
});
