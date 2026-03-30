// WarzonePhone — Pure JS client (Variant 1).
// WebSocket transport, raw PCM, no WASM, no FEC.
// Relies on wzp-core.js for UI and audio helpers.

'use strict';

class WZPPureClient {
  /**
   * @param {Object} options
   * @param {string} options.wsUrl       WebSocket URL (ws://host/ws/room)
   * @param {string} options.room        Room name
   * @param {Function} options.onAudio   callback(Int16Array) for playback
   * @param {Function} options.onStatus  callback(string) for UI status
   * @param {Function} options.onStats   callback({sent, recv, loss, elapsed}) for UI
   */
  constructor(options) {
    this.wsUrl = options.wsUrl;
    this.room = options.room;
    this.onAudio = options.onAudio || null;
    this.onStatus = options.onStatus || null;
    this.onStats = options.onStats || null;

    this.ws = null;
    this.sequence = 0;
    this.stats = { sent: 0, recv: 0 };
    this._startTime = 0;
    this._statsInterval = null;
    this._connected = false;
  }

  /**
   * Open WebSocket connection to the wzp-web bridge.
   * @returns {Promise<void>} resolves when connected
   */
  async connect() {
    if (this._connected) return;

    return new Promise((resolve, reject) => {
      this._status('Connecting to room: ' + this.room + '...');

      this.ws = new WebSocket(this.wsUrl);
      this.ws.binaryType = 'arraybuffer';

      this.ws.onopen = () => {
        this._connected = true;
        this.sequence = 0;
        this.stats = { sent: 0, recv: 0 };
        this._startTime = Date.now();
        this._status('Connected to room: ' + this.room);
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

      this.ws.onerror = (err) => {
        if (!this._connected) {
          this._cleanup();
          reject(new Error('WebSocket connection failed'));
        } else {
          this._status('Connection error');
        }
      };
    });
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
  }

  /**
   * Send a PCM audio frame over the WebSocket.
   * @param {ArrayBuffer} pcmBuffer  960-sample Int16 PCM (1920 bytes)
   */
  async sendAudio(pcmBuffer) {
    if (!this._connected || !this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return;
    }

    // Pure JS variant: send raw PCM directly (no encryption, no header).
    // The wzp-web bridge handles QUIC-side encryption.
    this.ws.send(pcmBuffer);
    this.sequence++;
    this.stats.sent++;
  }

  // -----------------------------------------------------------------------
  // Internal
  // -----------------------------------------------------------------------

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
      // Simple loss estimate: if we sent frames, the other side should
      // receive roughly the same count. Since we only see our own recv,
      // we report raw counts and let the UI decide.
      const loss = this.stats.sent > 0
        ? Math.max(0, 1 - this.stats.recv / this.stats.sent)
        : 0;
      if (this.onStats) {
        this.onStats({
          sent: this.stats.sent,
          recv: this.stats.recv,
          loss: loss,
          elapsed: elapsed,
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

window.WZPPureClient = WZPPureClient;
