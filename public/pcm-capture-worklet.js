// The only browser microphone processor used by Talk and the settings test. It
// batches raw mono floats; resampling, bounds and native inference remain in
// ordinary testable code outside the real-time audio thread.
//
// Served from `public/` rather than built into a `blob:` URL on the fly. An
// AudioWorklet module is fetched as a script, so the app's CSP decides whether
// it may load at all, and that CSP has no `script-src` — it falls back to
// `default-src 'self'`, which no `blob:` URL satisfies. `addModule()` was
// rejected, the worklet never installed, and the microphone produced no samples
// while every visible part of Talk still claimed to be ready. Same origin loads
// under `'self'`, so the capture path stops depending on a directive nobody
// looks at when they widen the others.
class LittleMonkeyPcmCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.pending = [];
  }
  process(inputs) {
    const channel = inputs[0] && inputs[0][0];
    if (!channel || channel.length === 0) return true;
    for (let i = 0; i < channel.length; i += 1) this.pending.push(channel[i]);
    if (this.pending.length >= 2048) {
      const frame = Float32Array.from(this.pending);
      this.pending = [];
      this.port.postMessage(frame, [frame.buffer]);
    }
    return true;
  }
}
registerProcessor('little-monkey-pcm-capture', LittleMonkeyPcmCapture);
