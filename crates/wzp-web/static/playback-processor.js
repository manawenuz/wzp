// AudioWorklet processor for playing received audio.
// Receives PCM samples from the main thread and outputs them.

class PlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buffer = new Float32Array(0);
    this.maxBuffered = 48000 / 5; // 200ms max
    this.port.onmessage = (e) => {
      const incoming = new Float32Array(e.data);
      // Append
      const newBuf = new Float32Array(this.buffer.length + incoming.length);
      newBuf.set(this.buffer);
      newBuf.set(incoming, this.buffer.length);
      this.buffer = newBuf;

      // Cap buffer to prevent drift
      if (this.buffer.length > this.maxBuffered) {
        this.buffer = this.buffer.slice(this.buffer.length - this.maxBuffered);
      }
    };
  }

  process(inputs, outputs, parameters) {
    const output = outputs[0];
    if (!output || !output[0]) return true;

    const out = output[0]; // 128 samples typically

    if (this.buffer.length >= out.length) {
      out.set(this.buffer.subarray(0, out.length));
      this.buffer = this.buffer.slice(out.length);
    } else if (this.buffer.length > 0) {
      out.set(this.buffer);
      for (let i = this.buffer.length; i < out.length; i++) out[i] = 0;
      this.buffer = new Float32Array(0);
    } else {
      for (let i = 0; i < out.length; i++) out[i] = 0;
    }

    return true;
  }
}

registerProcessor('playback-processor', PlaybackProcessor);
