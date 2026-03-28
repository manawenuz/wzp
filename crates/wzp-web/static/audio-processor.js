// WarzonePhone AudioWorklet processors.
// Both capture and playback handle 960-sample frames (20ms @ 48kHz).
// AudioWorklet calls process() with 128-sample blocks, so we buffer internally.

const FRAME_SIZE = 960;

class WZPCaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // Pre-allocate ring buffer large enough for several frames
    this._ring = new Float32Array(FRAME_SIZE * 4);
    this._writePos = 0;
  }

  process(inputs, _outputs, _parameters) {
    const input = inputs[0];
    if (!input || !input[0]) return true;

    const samples = input[0]; // Float32Array, 128 samples typically
    const len = samples.length;

    // Write into ring buffer
    if (this._writePos + len > this._ring.length) {
      // Should not happen with FRAME_SIZE * 4 capacity and timely draining,
      // but handle gracefully by resizing
      const bigger = new Float32Array(this._ring.length * 2);
      bigger.set(this._ring.subarray(0, this._writePos));
      this._ring = bigger;
    }
    this._ring.set(samples, this._writePos);
    this._writePos += len;

    // Drain complete 960-sample frames
    while (this._writePos >= FRAME_SIZE) {
      // Convert Float32 -> Int16 PCM
      const pcm = new Int16Array(FRAME_SIZE);
      for (let i = 0; i < FRAME_SIZE; i++) {
        const s = this._ring[i];
        pcm[i] = s < -1 ? -32768 : s > 1 ? 32767 : (s * 32767) | 0;
      }

      // Shift remaining data forward
      this._writePos -= FRAME_SIZE;
      if (this._writePos > 0) {
        this._ring.copyWithin(0, FRAME_SIZE, FRAME_SIZE + this._writePos);
      }

      // Send the Int16 PCM buffer (1920 bytes) to the main thread
      this.port.postMessage(pcm.buffer, [pcm.buffer]);
    }

    return true;
  }
}

class WZPPlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // Ring buffer for decoded Float32 samples ready for output
    this._ring = new Float32Array(FRAME_SIZE * 8);
    this._readPos = 0;
    this._writePos = 0;
    this._maxBuffered = FRAME_SIZE * 6; // ~120ms max to prevent drift

    this.port.onmessage = (e) => {
      // Receive Int16 PCM from main thread, convert to Float32
      const pcm = new Int16Array(e.data);
      const len = pcm.length;

      // Check capacity
      let available = this._writePos - this._readPos;
      if (available < 0) available += this._ring.length;
      if (available + len > this._maxBuffered) {
        // Too much buffered; drop oldest samples to prevent drift
        this._readPos = this._writePos;
      }

      // Ensure ring buffer is big enough
      if (this._ring.length < len + available + 128) {
        const bigger = new Float32Array(this._ring.length * 2);
        // Copy existing data contiguously
        if (this._readPos <= this._writePos) {
          bigger.set(this._ring.subarray(this._readPos, this._writePos));
        } else {
          const firstPart = this._ring.subarray(this._readPos);
          const secondPart = this._ring.subarray(0, this._writePos);
          bigger.set(firstPart);
          bigger.set(secondPart, firstPart.length);
        }
        this._ring = bigger;
        const count = available;
        this._readPos = 0;
        this._writePos = count;
      }

      // Write converted samples into ring buffer linearly (simpler: use linear buffer)
      for (let i = 0; i < len; i++) {
        this._ring[this._writePos] = pcm[i] / 32768.0;
        this._writePos++;
        if (this._writePos >= this._ring.length) this._writePos = 0;
      }
    };
  }

  process(_inputs, outputs, _parameters) {
    const output = outputs[0];
    if (!output || !output[0]) return true;

    const out = output[0]; // 128 samples typically
    const needed = out.length;

    let available;
    if (this._writePos >= this._readPos) {
      available = this._writePos - this._readPos;
    } else {
      available = this._ring.length - this._readPos + this._writePos;
    }

    if (available >= needed) {
      for (let i = 0; i < needed; i++) {
        out[i] = this._ring[this._readPos];
        this._readPos++;
        if (this._readPos >= this._ring.length) this._readPos = 0;
      }
    } else {
      // Output what we have, zero-fill the rest (underrun)
      for (let i = 0; i < available; i++) {
        out[i] = this._ring[this._readPos];
        this._readPos++;
        if (this._readPos >= this._ring.length) this._readPos = 0;
      }
      for (let i = available; i < needed; i++) {
        out[i] = 0;
      }
    }

    return true;
  }
}

registerProcessor('wzp-capture-processor', WZPCaptureProcessor);
registerProcessor('wzp-playback-processor', WZPPlaybackProcessor);
