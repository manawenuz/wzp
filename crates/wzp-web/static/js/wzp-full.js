// WarzonePhone — Full WASM + WebTransport client (Variant 3).
//
// Architecture:
//   - WebTransport for unreliable datagrams (UDP-like, no head-of-line blocking)
//   - ChaCha20-Poly1305 encryption via WASM (wzp-wasm WzpCryptoSession)
//   - RaptorQ FEC via WASM (wzp-wasm WzpFecEncoder/WzpFecDecoder)
//   - X25519 key exchange via WASM (wzp-wasm WzpKeyExchange)
//
// NOTE: WebTransport requires the relay to support HTTP/3 (h3-quinn).
// The current wzp-relay uses raw QUIC. This variant demonstrates the full
// architecture but will need relay-side HTTP/3 support to work end-to-end.
// For development / testing, use the hybrid variant (WebSocket + WASM FEC).
//
// Relies on wzp-core.js for UI and audio helpers.

'use strict';

const WZP_WASM_PATH = '/wasm/wzp_wasm.js';

// 12-byte MediaHeader size (matches wzp-proto MediaHeader::WIRE_SIZE).
const MEDIA_HEADER_SIZE = 12;

// FEC wire header: block_id(1) + symbol_idx(1) + is_repair(1) = 3 bytes.
const FEC_HEADER_SIZE = 3;

class WZPFullClient {
  /**
   * @param {Object} options
   * @param {string} options.url          WebTransport URL (https://host:port)
   * @param {string} options.room         Room name
   * @param {Function} options.onAudio    callback(Int16Array) for playback
   * @param {Function} options.onStatus   callback(string) for UI status
   * @param {Function} options.onStats    callback(Object) for UI stats
   */
  constructor(options) {
    this.url = options.url;
    this.room = options.room;
    this.onAudio = options.onAudio || null;
    this.onStatus = options.onStatus || null;
    this.onStats = options.onStats || null;

    this.wt = null;                  // WebTransport instance
    this.datagramWriter = null;      // WritableStreamDefaultWriter
    this.datagramReader = null;      // ReadableStreamDefaultReader
    this.cryptoSession = null;       // WzpCryptoSession (WASM)
    this.fecEncoder = null;          // WzpFecEncoder (WASM)
    this.fecDecoder = null;          // WzpFecDecoder (WASM)
    this.sequence = 0;
    this._wasmModule = null;
    this._connected = false;
    this._startTime = 0;
    this._statsInterval = null;
    this._recvLoopRunning = false;
    this.stats = { sent: 0, recv: 0, fecRecovered: 0, encrypted: 0, decrypted: 0 };
  }

  /**
   * Connect: load WASM, open WebTransport, perform key exchange,
   * initialise FEC, and start the receive loop.
   */
  async connect() {
    if (this._connected) return;

    // --- Guard: WebTransport support ---
    if (typeof WebTransport === 'undefined') {
      throw new Error(
        'WebTransport is not supported in this browser. ' +
        'Use the hybrid (?variant=hybrid) or pure (?variant=pure) variant instead.'
      );
    }

    this._status('Loading WASM module...');

    // 1. Load WASM
    this._wasmModule = await import(WZP_WASM_PATH);
    await this._wasmModule.default();

    this._status('Connecting via WebTransport to ' + this.url + '...');

    // 2. WebTransport connection
    //    The URL should include the room, e.g. https://host:port/room
    const wtUrl = this.url + '/' + encodeURIComponent(this.room);
    this.wt = new WebTransport(wtUrl);

    this.wt.closed.then(() => {
      const wasConnected = this._connected;
      this._cleanup();
      if (wasConnected) {
        this._status('WebTransport closed');
      }
    }).catch((err) => {
      this._cleanup();
      this._status('WebTransport error: ' + err.message);
    });

    await this.wt.ready;

    // 3. Get datagram streams (unreliable, QUIC DATAGRAM frames)
    this.datagramWriter = this.wt.datagrams.writable.getWriter();
    this.datagramReader = this.wt.datagrams.readable.getReader();

    // 4. Key exchange over a bidirectional stream
    this._status('Performing key exchange...');
    await this._performKeyExchange();

    // 5. Initialise FEC (5 source symbols per block, 256-byte symbols)
    this.fecEncoder = new this._wasmModule.WzpFecEncoder(5, 256);
    this.fecDecoder = new this._wasmModule.WzpFecDecoder(5, 256);

    this._connected = true;
    this.sequence = 0;
    this.stats = { sent: 0, recv: 0, fecRecovered: 0, encrypted: 0, decrypted: 0 };
    this._startTime = Date.now();
    this._startStatsTimer();

    // 6. Start receive loop (runs until disconnect)
    this._recvLoop();

    this._status('Connected to room: ' + this.room + ' (encrypted, FEC active)');
  }

  /**
   * Disconnect and clean up all resources.
   */
  disconnect() {
    this._connected = false;
    if (this.wt) {
      try { this.wt.close(); } catch (_) { /* ignore */ }
      this.wt = null;
    }
    this._cleanup();
  }

  /**
   * Send a PCM audio frame.
   *
   * Pipeline: PCM -> FEC encode -> encrypt -> datagram send.
   *
   * @param {ArrayBuffer} pcmBuffer  960-sample Int16 PCM (1920 bytes)
   */
  async sendAudio(pcmBuffer) {
    if (!this._connected || !this.datagramWriter || !this.cryptoSession) return;

    const pcmBytes = new Uint8Array(pcmBuffer);

    // Build a minimal 12-byte MediaHeader for AAD.
    const header = this._buildMediaHeader(this.sequence);

    // FEC encode: feed the frame; when a block completes we get wire packets.
    const fecOutput = this.fecEncoder.add_symbol(pcmBytes);

    if (fecOutput) {
      // FEC block completed — send all packets (source + repair).
      const packetSize = FEC_HEADER_SIZE + 256; // header + symbol_size
      for (let offset = 0; offset + packetSize <= fecOutput.length; offset += packetSize) {
        const fecPacket = fecOutput.slice(offset, offset + packetSize);

        // Encrypt: header bytes as AAD, FEC packet as plaintext.
        const ciphertext = this.cryptoSession.encrypt(header, fecPacket);
        this.stats.encrypted++;

        // Build wire datagram: header (12) + ciphertext
        const datagram = new Uint8Array(MEDIA_HEADER_SIZE + ciphertext.length);
        datagram.set(header, 0);
        datagram.set(ciphertext, MEDIA_HEADER_SIZE);

        try {
          await this.datagramWriter.write(datagram);
        } catch (e) {
          // Datagram send can fail if the transport is closing.
          if (this._connected) {
            console.warn('[wzp-full] datagram write failed:', e);
          }
          return;
        }
        this.stats.sent++;
      }
    }
    // If FEC block not yet complete, accumulate (no packets sent yet).

    this.sequence = (this.sequence + 1) & 0xFFFF;
  }

  /**
   * Test crypto + FEC roundtrip entirely in WASM (no network).
   * Useful for verifying the WASM module works correctly in the browser.
   *
   * @returns {Object} test results
   */
  testCryptoFec() {
    if (!this._wasmModule) {
      return { success: false, error: 'WASM module not loaded' };
    }

    const t0 = performance.now();
    const wasm = this._wasmModule;

    // Key exchange
    const alice = new wasm.WzpKeyExchange();
    const bob = new wasm.WzpKeyExchange();
    const aliceSecret = alice.derive_shared_secret(bob.public_key());
    const bobSecret = bob.derive_shared_secret(alice.public_key());

    // Verify secrets match
    let secretsMatch = aliceSecret.length === bobSecret.length;
    if (secretsMatch) {
      for (let i = 0; i < aliceSecret.length; i++) {
        if (aliceSecret[i] !== bobSecret[i]) { secretsMatch = false; break; }
      }
    }

    // Encrypt/decrypt
    const aliceSession = new wasm.WzpCryptoSession(aliceSecret);
    const bobSession = new wasm.WzpCryptoSession(bobSecret);

    const header = new Uint8Array([0xDE, 0xAD, 0xBE, 0xEF]);
    const plaintext = new TextEncoder().encode('hello warzone from full variant');

    const ciphertext = aliceSession.encrypt(header, plaintext);
    const decrypted = bobSession.decrypt(header, ciphertext);

    let cryptoOk = decrypted.length === plaintext.length;
    if (cryptoOk) {
      for (let i = 0; i < plaintext.length; i++) {
        if (decrypted[i] !== plaintext[i]) { cryptoOk = false; break; }
      }
    }

    // FEC test (same as hybrid testFec)
    const encoder = new wasm.WzpFecEncoder(5, 256);
    const decoder = new wasm.WzpFecDecoder(5, 256);

    const frames = [];
    for (let i = 0; i < 5; i++) {
      const frame = new Uint8Array(100);
      for (let j = 0; j < 100; j++) frame[j] = ((i * 37 + 7) + j) & 0xFF;
      frames.push(frame);
    }

    let wireData = null;
    for (const frame of frames) {
      const result = encoder.add_symbol(frame);
      if (result) wireData = result;
    }

    const PACKET_SIZE = FEC_HEADER_SIZE + 256;
    const packets = [];
    if (wireData) {
      for (let off = 0; off + PACKET_SIZE <= wireData.length; off += PACKET_SIZE) {
        packets.push({
          blockId: wireData[off],
          symbolIdx: wireData[off + 1],
          isRepair: wireData[off + 2] !== 0,
          data: wireData.slice(off + FEC_HEADER_SIZE, off + PACKET_SIZE),
        });
      }
    }

    // Drop 2 packets, try to recover
    let fecDecoded = null;
    for (let i = 0; i < packets.length; i++) {
      if (i === 1 || i === 3) continue; // simulate loss
      const pkt = packets[i];
      const result = decoder.add_symbol(pkt.blockId, pkt.symbolIdx, pkt.isRepair, pkt.data);
      if (result) { fecDecoded = result; break; }
    }

    let fecOk = false;
    if (fecDecoded) {
      const expected = new Uint8Array(5 * 100);
      let off = 0;
      for (const f of frames) { expected.set(f, off); off += f.length; }
      fecOk = fecDecoded.length === expected.length;
      if (fecOk) {
        for (let i = 0; i < expected.length; i++) {
          if (fecDecoded[i] !== expected[i]) { fecOk = false; break; }
        }
      }
    }

    // Cleanup WASM objects
    alice.free();
    bob.free();
    aliceSession.free();
    bobSession.free();
    encoder.free();
    decoder.free();

    const elapsed = performance.now() - t0;

    return {
      success: secretsMatch && cryptoOk && fecOk,
      secretsMatch,
      cryptoOk,
      fecOk,
      fecPacketsTotal: packets.length,
      fecDropped: 2,
      elapsed: elapsed.toFixed(2) + 'ms',
    };
  }

  // =========================================================================
  // Internal
  // =========================================================================

  /**
   * Perform X25519 key exchange over a WebTransport bidirectional stream.
   *
   * Protocol (simplified DH, not the full SignalMessage handshake):
   *   1. Open a bidirectional stream.
   *   2. Send our 32-byte X25519 public key.
   *   3. Read the peer's 32-byte public key.
   *   4. Derive shared secret via HKDF.
   *   5. Create WzpCryptoSession from the shared secret.
   *
   * In production this would use the full SignalMessage protocol over the
   * bidirectional stream (offer/answer/encrypted-session). For now we do
   * a simple DH swap to prove the architecture.
   */
  async _performKeyExchange() {
    const wasm = this._wasmModule;
    const kx = new wasm.WzpKeyExchange();
    const ourPub = kx.public_key(); // Uint8Array(32)

    // Open a bidirectional stream for signaling.
    const stream = await this.wt.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    const reader = stream.readable.getReader();

    // Send our public key.
    await writer.write(new Uint8Array(ourPub));

    // Read peer's public key (exactly 32 bytes).
    // WebTransport streams are byte-oriented; we may get it in chunks.
    let peerPub = new Uint8Array(0);
    while (peerPub.length < 32) {
      const { value, done } = await reader.read();
      if (done) {
        throw new Error('Key exchange stream closed before receiving peer public key');
      }
      const combined = new Uint8Array(peerPub.length + value.length);
      combined.set(peerPub, 0);
      combined.set(value, peerPub.length);
      peerPub = combined;
    }
    peerPub = peerPub.slice(0, 32);

    // Derive shared secret and create crypto session.
    const secret = kx.derive_shared_secret(peerPub);
    this.cryptoSession = new wasm.WzpCryptoSession(secret);

    // Close the signaling stream (key exchange complete).
    try {
      writer.releaseLock();
      reader.releaseLock();
      await stream.writable.close();
    } catch (_) {
      // Best-effort close.
    }

    kx.free();
  }

  /**
   * Receive loop: read datagrams, decrypt, FEC decode, play audio.
   *
   * Runs until the transport closes or disconnect() is called.
   */
  async _recvLoop() {
    if (this._recvLoopRunning) return;
    this._recvLoopRunning = true;

    try {
      while (this._connected && this.datagramReader) {
        const { value, done } = await this.datagramReader.read();
        if (done) break;

        this.stats.recv++;

        // value is a Uint8Array datagram: header(12) + ciphertext
        if (value.length <= MEDIA_HEADER_SIZE) continue; // too short

        const headerAad = value.slice(0, MEDIA_HEADER_SIZE);
        const ciphertext = value.slice(MEDIA_HEADER_SIZE);

        // Decrypt
        let fecPacket;
        try {
          fecPacket = this.cryptoSession.decrypt(headerAad, ciphertext);
          this.stats.decrypted++;
        } catch (e) {
          // Decryption failure — corrupted or out-of-order packet.
          // In a real implementation we'd handle sequence number gaps.
          console.warn('[wzp-full] decrypt failed:', e);
          continue;
        }

        // FEC decode: parse the FEC wire header and feed to decoder.
        if (fecPacket.length < FEC_HEADER_SIZE) continue;
        const blockId = fecPacket[0];
        const symbolIdx = fecPacket[1];
        const isRepair = fecPacket[2] !== 0;
        const symbolData = fecPacket.slice(FEC_HEADER_SIZE);

        const decoded = this.fecDecoder.add_symbol(blockId, symbolIdx, isRepair, symbolData);
        if (decoded) {
          this.stats.fecRecovered++;
          // decoded is concatenated original PCM frames.
          // Each frame is 1920 bytes (960 Int16 samples @ 48kHz mono).
          const FRAME_BYTES = 1920;
          for (let off = 0; off + FRAME_BYTES <= decoded.length; off += FRAME_BYTES) {
            const pcmSlice = decoded.slice(off, off + FRAME_BYTES);
            const pcm = new Int16Array(pcmSlice.buffer, pcmSlice.byteOffset, pcmSlice.byteLength / 2);
            if (this.onAudio) {
              this.onAudio(pcm);
            }
          }
        }
      }
    } catch (e) {
      if (this._connected) {
        console.warn('[wzp-full] recv loop error:', e);
      }
    } finally {
      this._recvLoopRunning = false;
    }
  }

  /**
   * Build a minimal 12-byte MediaHeader for use as AAD.
   *
   * Wire layout (from wzp-proto::packet::MediaHeader):
   *   Byte 0:  V(1)|T(1)|CodecID(4)|Q(1)|FecRatioHi(1)
   *   Byte 1:  FecRatioLo(6)|unused(2)
   *   Bytes 2-3: Sequence number (BE u16)
   *   Bytes 4-7: Timestamp ms (BE u32)
   *   Byte 8:  FEC block ID
   *   Byte 9:  FEC symbol index
   *   Byte 10: Reserved
   *   Byte 11: CSRC count
   *
   * @param {number} seq  Sequence number (u16)
   * @returns {Uint8Array} 12-byte header
   */
  _buildMediaHeader(seq) {
    const buf = new Uint8Array(MEDIA_HEADER_SIZE);
    // Byte 0: version=0, is_repair=0, codec=0 (Opus), quality_report=0, fec_ratio_hi=0
    buf[0] = 0x00;
    // Byte 1: fec_ratio_lo=0
    buf[1] = 0x00;
    // Bytes 2-3: sequence (BE u16)
    buf[2] = (seq >> 8) & 0xFF;
    buf[3] = seq & 0xFF;
    // Bytes 4-7: timestamp (BE u32) — ms since session start
    const ts = Date.now() - this._startTime;
    buf[4] = (ts >> 24) & 0xFF;
    buf[5] = (ts >> 16) & 0xFF;
    buf[6] = (ts >> 8) & 0xFF;
    buf[7] = ts & 0xFF;
    // Bytes 8-11: FEC block/symbol/reserved/csrc — filled by FEC layer in production
    return buf;
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
          loss,
          elapsed,
          encrypted: this.stats.encrypted,
          decrypted: this.stats.decrypted,
          fecRecovered: this.stats.fecRecovered,
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
    this.datagramWriter = null;
    this.datagramReader = null;
    if (this.cryptoSession) {
      try { this.cryptoSession.free(); } catch (_) { /* ignore */ }
      this.cryptoSession = null;
    }
    if (this.fecEncoder) {
      try { this.fecEncoder.free(); } catch (_) { /* ignore */ }
      this.fecEncoder = null;
    }
    if (this.fecDecoder) {
      try { this.fecDecoder.free(); } catch (_) { /* ignore */ }
      this.fecDecoder = null;
    }
  }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

window.WZPFullClient = WZPFullClient;
