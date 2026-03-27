// AudioWorklet processor for capturing microphone audio.
// Accumulates samples and posts 960-sample (20ms @ 48kHz) frames to the main thread.

class CaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buffer = new Float32Array(0);
  }

  process(inputs, outputs, parameters) {
    const input = inputs[0];
    if (!input || !input[0]) return true;

    const samples = input[0]; // Float32Array, typically 128 samples

    // Accumulate
    const newBuf = new Float32Array(this.buffer.length + samples.length);
    newBuf.set(this.buffer);
    newBuf.set(samples, this.buffer.length);
    this.buffer = newBuf;

    // Send complete 960-sample frames
    while (this.buffer.length >= 960) {
      const frame = this.buffer.slice(0, 960);
      this.buffer = this.buffer.slice(960);

      // Convert to Int16
      const pcm = new Int16Array(960);
      for (let i = 0; i < 960; i++) {
        pcm[i] = Math.max(-32768, Math.min(32767, Math.round(frame[i] * 32767)));
      }
      this.port.postMessage(pcm.buffer, [pcm.buffer]);
    }

    return true;
  }
}

registerProcessor('capture-processor', CaptureProcessor);
