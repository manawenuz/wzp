// WarzonePhone — Hybrid JS + WASM client (Variant 2).
// WebSocket transport, raw PCM, WASM FEC (RaptorQ) ready for WebTransport.
// Relies on wzp-core.js for UI and audio helpers.
//
// The WASM FEC module is loaded and exposed but not used on the wire yet,
// because WebSocket is TCP (no packet loss). FEC will activate when
// WebTransport (UDP) is added. A testFec() method demonstrates FEC
// encode -> simulate loss -> decode in the browser.

'use strict';

// WASM module path (served from /wasm/ by the wzp-web bridge).
const WZP_WASM_PATH = '/wasm/wzp_wasm.js';

class WZPHybridClient {
  /**
   * @param {Object} options
   * @param {string} options.wsUrl       WebSocket URL (ws://host/ws/room)
   * @param {string} options.room        Room name
   * @param {Function} options.onAudio   callback(Int16Array) for playback
   * @param {Function} options.onStatus  callback(string) for UI status
   * @param {Function} options.onStats   callback({sent, recv, loss, elapsed, fecRecovered}) for UI
   */
  constructor(options) {
    this.wsUrl = options.wsUrl;
    this.room = options.room;
    this.onAudio = options.onAudio || null;
    this.onStatus = options.onStatus || null;
    this.onStats = options.onStats || null;

    this.ws = null;
    this.sequence = 0;
    this.stats = { sent: 0, recv: 0, fecRecovered: 0 };
    this._startTime = 0;
    this._statsInterval = null;
    this._connected = false;

    // WASM FEC instances (loaded in connect()).
    this._wasmModule = null;
    this.fecEncoder = null;
    this.fecDecoder = null;
    this._fecReady = false;
  }

  /**
   * Open WebSocket connection and load the WASM FEC module.
   * @returns {Promise<void>} resolves when connected
   */
  async connect() {
    if (this._connected) return;

    // Load WASM module in parallel with WebSocket connect.
    const wasmPromise = this._loadWasm();

    const wsPromise = new Promise((resolve, reject) => {
      this._status('Connecting to room: ' + this.room + '...');

      this.ws = new WebSocket(this.wsUrl);
      this.ws.binaryType = 'arraybuffer';

      this.ws.onopen = () => {
        this._connected = true;
        this.sequence = 0;
        this.stats = { sent: 0, recv: 0, fecRecovered: 0 };
        this._startTime = Date.now();
        this._startStatsTimer();
        resolve();
      };

      this.ws.onmessage = (event) => {
        this._handleMessage(event);
      };

      this.ws.onclose = () => {
        const wasConnected = this._connected;
        this._cleanup();
        if (wasConnected) {
          this._status('Disconnected');
        }
      };

      this.ws.onerror = () => {
        if (!this._connected) {
          this._cleanup();
          reject(new Error('WebSocket connection failed'));
        } else {
          this._status('Connection error');
        }
      };
    });

    // Wait for both WASM load and WS connect.
    await Promise.all([wasmPromise, wsPromise]);

    const fecStatus = this._fecReady ? 'FEC ready' : 'FEC unavailable';
    this._status('Connected to room: ' + this.room + ' (' + fecStatus + ')');
  }

  /**
   * Close WebSocket and clean up.
   */
  disconnect() {
    this._connected = false;
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
    this._stopStatsTimer();
    // Keep WASM module loaded (reusable).
    this.fecEncoder = null;
    this.fecDecoder = null;
  }

  /**
   * Send a PCM audio frame over the WebSocket.
   * Currently sends raw PCM (same as pure client) since WebSocket is TCP.
   * When WebTransport is added, this will FEC-encode before sending.
   * @param {ArrayBuffer} pcmBuffer  960-sample Int16 PCM (1920 bytes)
   */
  async sendAudio(pcmBuffer) {
    if (!this._connected || !this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return;
    }

    // Over WebSocket (TCP): send raw PCM, no FEC needed.
    // Over WebTransport (UDP, future): would call this.fecEncoder.add_symbol()
    // and send the resulting FEC-protected packets.
    this.ws.send(pcmBuffer);
    this.sequence++;
    this.stats.sent++;
  }

  /**
   * Test FEC encode -> simulate loss -> decode in the browser.
   * Demonstrates that the WASM RaptorQ module works correctly.
   *
   * @param {Object} [opts]
   * @param {number} [opts.blockSize=5]     Source symbols per block
   * @param {number} [opts.symbolSize=256]  Padded symbol size
   * @param {number} [opts.frameSize=100]   Bytes per test frame
   * @param {number} [opts.dropCount=2]     Number of packets to drop
   * @returns {Object} { success, sourcePackets, repairPackets, dropped, recovered, elapsed }
   */
  testFec(opts) {
    if (!this._fecReady) {
      return { success: false, error: 'WASM FEC module not loaded' };
    }

    const blockSize = (opts && opts.blockSize) || 5;
    const symbolSize = (opts && opts.symbolSize) || 256;
    const frameSize = (opts && opts.frameSize) || 100;
    const dropCount = (opts && opts.dropCount) || 2;

    const HEADER_SIZE = 3; // block_id + symbol_idx + is_repair
    const packetSize = HEADER_SIZE + symbolSize;

    const t0 = performance.now();

    // Create fresh encoder/decoder for the test.
    const encoder = new this._wasmModule.WzpFecEncoder(blockSize, symbolSize);
    const decoder = new this._wasmModule.WzpFecDecoder(blockSize, symbolSize);

    // Generate test frames with known data.
    const frames = [];
    for (let i = 0; i < blockSize; i++) {
      const frame = new Uint8Array(frameSize);
      for (let j = 0; j < frameSize; j++) {
        frame[j] = ((i * 37 + 7) + j) & 0xFF;
      }
      frames.push(frame);
    }

    // Encode: feed frames to encoder; last one triggers block output.
    let wireData = null;
    for (const frame of frames) {
      const result = encoder.add_symbol(frame);
      if (result) {
        wireData = result;
      }
    }

    if (!wireData) {
      // Flush if block didn't complete (shouldn't happen with exact blockSize).
      wireData = encoder.flush();
    }

    // Parse wire packets.
    const packets = [];
    for (let offset = 0; offset + packetSize <= wireData.length; offset += packetSize) {
      packets.push({
        blockId: wireData[offset],
        symbolIdx: wireData[offset + 1],
        isRepair: wireData[offset + 2] !== 0,
        data: wireData.slice(offset + HEADER_SIZE, offset + packetSize),
      });
    }

    const sourcePackets = packets.filter(p => !p.isRepair).length;
    const repairPackets = packets.filter(p => p.isRepair).length;

    // Simulate packet loss: drop `dropCount` packets from the front (source symbols).
    const dropped = [];
    const surviving = [];
    for (let i = 0; i < packets.length; i++) {
      if (i < dropCount) {
        dropped.push(i);
      } else {
        surviving.push(packets[i]);
      }
    }

    // Decode from surviving packets.
    let decoded = null;
    for (const pkt of surviving) {
      const result = decoder.add_symbol(pkt.blockId, pkt.symbolIdx, pkt.isRepair, pkt.data);
      if (result) {
        decoded = result;
        break;
      }
    }

    const elapsed = performance.now() - t0;

    // Verify decoded data matches original frames.
    let success = false;
    if (decoded) {
      const expected = new Uint8Array(blockSize * frameSize);
      let off = 0;
      for (const frame of frames) {
        expected.set(frame, off);
        off += frame.length;
      }

      success = decoded.length === expected.length;
      if (success) {
        for (let i = 0; i < decoded.length; i++) {
          if (decoded[i] !== expected[i]) {
            success = false;
            break;
          }
        }
      }
    }

    // Free WASM objects.
    encoder.free();
    decoder.free();

    return {
      success,
      sourcePackets,
      repairPackets,
      totalPackets: packets.length,
      dropped: dropCount,
      recovered: success,
      decodedBytes: decoded ? decoded.length : 0,
      expectedBytes: blockSize * frameSize,
      elapsed: elapsed.toFixed(2) + 'ms',
    };
  }

  // -----------------------------------------------------------------------
  // Internal
  // -----------------------------------------------------------------------

  async _loadWasm() {
    try {
      // Dynamic import of the wasm-pack generated JS glue.
      this._wasmModule = await import(WZP_WASM_PATH);
      // Initialize the WASM module (calls __wbg_init).
      await this._wasmModule.default();

      // Create FEC encoder/decoder instances.
      // 5 symbols per block, 256-byte symbols — matches native wzp-fec defaults.
      this.fecEncoder = new this._wasmModule.WzpFecEncoder(5, 256);
      this.fecDecoder = new this._wasmModule.WzpFecDecoder(5, 256);
      this._fecReady = true;

      console.log('[wzp-hybrid] WASM FEC module loaded successfully');
    } catch (e) {
      console.warn('[wzp-hybrid] WASM FEC module failed to load:', e);
      this._fecReady = false;
      // Non-fatal: client still works without FEC (like pure variant).
    }
  }

  _handleMessage(event) {
    if (!(event.data instanceof ArrayBuffer)) return;
    const pcm = new Int16Array(event.data);
    this.stats.recv++;
    if (this.onAudio) {
      this.onAudio(pcm);
    }
  }

  _startStatsTimer() {
    this._stopStatsTimer();
    this._statsInterval = setInterval(() => {
      if (!this._connected) {
        this._stopStatsTimer();
        return;
      }
      const elapsed = (Date.now() - this._startTime) / 1000;
      const loss = this.stats.sent > 0
        ? Math.max(0, 1 - this.stats.recv / this.stats.sent)
        : 0;
      if (this.onStats) {
        this.onStats({
          sent: this.stats.sent,
          recv: this.stats.recv,
          loss: loss,
          elapsed: elapsed,
          fecRecovered: this.stats.fecRecovered,
          fecReady: this._fecReady,
        });
      }
    }, 1000);
  }

  _stopStatsTimer() {
    if (this._statsInterval) {
      clearInterval(this._statsInterval);
      this._statsInterval = null;
    }
  }

  _status(msg) {
    if (this.onStatus) this.onStatus(msg);
  }

  _cleanup() {
    this._connected = false;
    this._stopStatsTimer();
    if (this.ws) {
      try { this.ws.close(); } catch (_) { /* ignore */ }
      this.ws = null;
    }
  }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

window.WZPHybridClient = WZPHybridClient;
