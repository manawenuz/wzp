// WarzonePhone — Shared UI logic for all client variants.
// Provides: audio context management, mic capture, playback, UI wiring.

'use strict';

const WZP_SAMPLE_RATE = 48000;
const WZP_FRAME_SIZE = 960; // 20ms @ 48kHz

// ---------------------------------------------------------------------------
// Variant detection
// ---------------------------------------------------------------------------

function wzpDetectVariant() {
  const params = new URLSearchParams(location.search);
  const v = (params.get('variant') || 'pure').toLowerCase();
  if (v === 'hybrid' || v === 'full') return v;
  return 'pure';
}

// ---------------------------------------------------------------------------
// Room helpers
// ---------------------------------------------------------------------------

function wzpGetRoom() {
  const path = location.pathname.replace(/^\//, '').replace(/\/$/, '');
  if (path && path !== 'index.html') return path;
  const hash = location.hash.replace('#', '');
  if (hash) return hash;
  const el = document.getElementById('room');
  return (el && el.value.trim()) || 'default';
}

function wzpPrefillRoom() {
  const path = location.pathname.replace(/^\//, '').replace(/\/$/, '');
  if (path && path !== 'index.html') {
    const el = document.getElementById('room');
    if (el) el.value = path;
  }
}

// ---------------------------------------------------------------------------
// Status / stats helpers
// ---------------------------------------------------------------------------

function wzpUpdateStatus(msg) {
  const el = document.getElementById('status');
  if (el) el.textContent = msg;
}

function wzpUpdateStats(stats) {
  const el = document.getElementById('stats');
  if (!el) return;
  if (typeof stats === 'string') {
    el.textContent = stats;
  } else {
    const parts = [];
    if (stats.elapsed != null) parts.push(stats.elapsed.toFixed(1) + 's');
    if (stats.sent != null) parts.push('sent: ' + stats.sent);
    if (stats.recv != null) parts.push('recv: ' + stats.recv);
    if (stats.loss != null) parts.push('loss: ' + (stats.loss * 100).toFixed(1) + '%');
    if (stats.fecRecovered != null && stats.fecRecovered > 0) parts.push('fec: ' + stats.fecRecovered);
    if (stats.fecReady != null) parts.push(stats.fecReady ? 'FEC:on' : 'FEC:off');
    el.textContent = parts.join(' | ');
  }
}

function wzpUpdateLevel(pcmInt16) {
  const bar = document.getElementById('levelBar');
  if (!bar) return;
  let max = 0;
  for (let i = 0; i < pcmInt16.length; i += 16) {
    const v = Math.abs(pcmInt16[i]);
    if (v > max) max = v;
  }
  bar.style.width = (max / 32768 * 100) + '%';
}

// ---------------------------------------------------------------------------
// Audio context + worklet
// ---------------------------------------------------------------------------

let _wzpAudioCtx = null;
let _wzpWorkletLoaded = false;

async function wzpStartAudioContext() {
  if (_wzpAudioCtx && _wzpAudioCtx.state !== 'closed') return _wzpAudioCtx;
  _wzpAudioCtx = new AudioContext({ sampleRate: WZP_SAMPLE_RATE });
  _wzpWorkletLoaded = false;
  return _wzpAudioCtx;
}

function wzpGetAudioContext() {
  return _wzpAudioCtx;
}

async function _wzpLoadWorklet(audioCtx) {
  if (_wzpWorkletLoaded) return true;
  if (typeof AudioWorkletNode === 'undefined' || !audioCtx.audioWorklet) {
    console.warn('[wzp-core] AudioWorklet not supported, will use fallback');
    return false;
  }
  try {
    await audioCtx.audioWorklet.addModule('audio-processor.js');
    _wzpWorkletLoaded = true;
    return true;
  } catch (e) {
    console.warn('[wzp-core] AudioWorklet load failed:', e);
    return false;
  }
}

// ---------------------------------------------------------------------------
// Mic capture — returns { node, stop() }
// onFrame(ArrayBuffer) called for each 960-sample Int16 PCM frame
// ---------------------------------------------------------------------------

async function wzpConnectCapture(audioCtx, onFrame) {
  let mediaStream;
  try {
    mediaStream = await navigator.mediaDevices.getUserMedia({
      audio: {
        sampleRate: WZP_SAMPLE_RATE,
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: true,
      },
    });
  } catch (e) {
    throw new Error('Mic access denied: ' + e.message);
  }

  const source = audioCtx.createMediaStreamSource(mediaStream);
  const hasWorklet = await _wzpLoadWorklet(audioCtx);
  let captureNode;

  if (hasWorklet) {
    captureNode = new AudioWorkletNode(audioCtx, 'wzp-capture-processor');
    captureNode.port.onmessage = (e) => {
      onFrame(e.data); // ArrayBuffer of Int16 PCM
    };
    source.connect(captureNode);
    captureNode.connect(audioCtx.destination); // keep worklet alive
  } else {
    // ScriptProcessorNode fallback
    captureNode = audioCtx.createScriptProcessor(4096, 1, 1);
    let acc = new Float32Array(0);
    captureNode.onaudioprocess = (ev) => {
      const input = ev.inputBuffer.getChannelData(0);
      const n = new Float32Array(acc.length + input.length);
      n.set(acc);
      n.set(input, acc.length);
      acc = n;
      while (acc.length >= WZP_FRAME_SIZE) {
        const frame = acc.slice(0, WZP_FRAME_SIZE);
        acc = acc.slice(WZP_FRAME_SIZE);
        const pcm = new Int16Array(WZP_FRAME_SIZE);
        for (let i = 0; i < WZP_FRAME_SIZE; i++) {
          pcm[i] = Math.max(-32768, Math.min(32767, Math.round(frame[i] * 32767)));
        }
        onFrame(pcm.buffer);
      }
    };
    source.connect(captureNode);
    captureNode.connect(audioCtx.destination);
  }

  return {
    node: captureNode,
    stop() {
      captureNode.disconnect();
      mediaStream.getTracks().forEach((t) => t.stop());
    },
  };
}

// ---------------------------------------------------------------------------
// Playback — returns { node, play(Int16Array), stop() }
// ---------------------------------------------------------------------------

async function wzpConnectPlayback(audioCtx) {
  const hasWorklet = await _wzpLoadWorklet(audioCtx);
  let playbackNode;
  let nextPlayTime = 0;

  if (hasWorklet) {
    playbackNode = new AudioWorkletNode(audioCtx, 'wzp-playback-processor');
    playbackNode.connect(audioCtx.destination);
    return {
      node: playbackNode,
      play(pcmInt16) {
        // Transfer Int16 buffer to worklet
        const buf = pcmInt16.buffer.slice(
          pcmInt16.byteOffset,
          pcmInt16.byteOffset + pcmInt16.byteLength
        );
        playbackNode.port.postMessage(buf, [buf]);
      },
      stop() {
        playbackNode.disconnect();
      },
    };
  }

  // Fallback: scheduled BufferSource
  return {
    node: null,
    play(pcmInt16) {
      if (!audioCtx || audioCtx.state === 'closed') return;
      const floatData = new Float32Array(pcmInt16.length);
      for (let i = 0; i < pcmInt16.length; i++) {
        floatData[i] = pcmInt16[i] / 32768.0;
      }
      const buffer = audioCtx.createBuffer(1, floatData.length, WZP_SAMPLE_RATE);
      buffer.getChannelData(0).set(floatData);
      const source = audioCtx.createBufferSource();
      source.buffer = buffer;
      source.connect(audioCtx.destination);
      const now = audioCtx.currentTime;
      if (nextPlayTime < now || nextPlayTime > now + 1.0) {
        nextPlayTime = now + 0.02;
      }
      source.start(nextPlayTime);
      nextPlayTime += buffer.duration;
    },
    stop() {
      // nothing to disconnect for fallback
    },
  };
}

// ---------------------------------------------------------------------------
// UI wiring — call after DOM ready
// ---------------------------------------------------------------------------

function wzpInitUI(callbacks) {
  // callbacks: { onConnect(room), onDisconnect() }
  const btn = document.getElementById('callBtn');
  const pttBtn = document.getElementById('pttBtn');
  const pttCheckbox = document.getElementById('pttMode');
  let connected = false;
  let pttMode = false;

  wzpPrefillRoom();

  // Variant badge
  const variant = wzpDetectVariant();
  const badge = document.getElementById('variantBadge');
  if (badge) badge.textContent = variant.toUpperCase();

  // Variant selector radio buttons
  document.querySelectorAll('input[name="variant"]').forEach((radio) => {
    if (radio.value === variant) radio.checked = true;
    radio.addEventListener('change', () => {
      if (radio.checked) {
        const params = new URLSearchParams(location.search);
        params.set('variant', radio.value);
        location.search = params.toString();
      }
    });
  });

  btn.onclick = () => {
    if (connected) {
      connected = false;
      btn.textContent = 'Connect';
      btn.classList.remove('active');
      _showControls(false);
      if (callbacks.onDisconnect) callbacks.onDisconnect();
    } else {
      const room = wzpGetRoom();
      if (!room) {
        wzpUpdateStatus('Enter a room name');
        return;
      }
      connected = true;
      btn.disabled = true;
      if (callbacks.onConnect) callbacks.onConnect(room);
    }
  };

  // PTT toggle
  if (pttCheckbox) {
    pttCheckbox.onchange = () => {
      pttMode = pttCheckbox.checked;
      if (pttMode) {
        pttBtn.style.display = 'block';
        if (callbacks.onTransmit) callbacks.onTransmit(false);
      } else {
        pttBtn.style.display = 'none';
        if (callbacks.onTransmit) callbacks.onTransmit(true);
      }
    };
  }

  // PTT button events
  function startTx() {
    if (!pttMode || !connected) return;
    pttBtn.classList.add('transmitting');
    pttBtn.textContent = 'Transmitting...';
    if (callbacks.onTransmit) callbacks.onTransmit(true);
  }
  function stopTx() {
    if (!pttMode) return;
    pttBtn.classList.remove('transmitting');
    pttBtn.textContent = 'Hold to Talk';
    if (callbacks.onTransmit) callbacks.onTransmit(false);
  }

  if (pttBtn) {
    pttBtn.addEventListener('mousedown', startTx);
    pttBtn.addEventListener('mouseup', stopTx);
    pttBtn.addEventListener('mouseleave', stopTx);
    pttBtn.addEventListener('touchstart', (e) => { e.preventDefault(); startTx(); });
    pttBtn.addEventListener('touchend', (e) => { e.preventDefault(); stopTx(); });
  }

  // Spacebar PTT
  document.addEventListener('keydown', (e) => {
    if (pttMode && connected && e.code === 'Space' && !e.repeat) {
      e.preventDefault();
      startTx();
    }
  });
  document.addEventListener('keyup', (e) => {
    if (pttMode && connected && e.code === 'Space') {
      e.preventDefault();
      stopTx();
    }
  });

  function _showControls(show) {
    const controls = document.getElementById('controls');
    if (controls) controls.style.display = show ? 'flex' : 'none';
    if (!show && pttBtn) {
      pttBtn.style.display = 'none';
      pttMode = false;
      if (pttCheckbox) pttCheckbox.checked = false;
    }
  }

  return {
    setConnected(isConnected) {
      connected = isConnected;
      btn.disabled = false;
      if (isConnected) {
        btn.textContent = 'Disconnect';
        btn.classList.add('active');
        _showControls(true);
      } else {
        btn.textContent = 'Connect';
        btn.classList.remove('active');
        _showControls(false);
      }
    },
    isPTT() {
      return pttMode;
    },
  };
}

// ---------------------------------------------------------------------------
// Exports (global)
// ---------------------------------------------------------------------------

window.WZPCore = {
  SAMPLE_RATE: WZP_SAMPLE_RATE,
  FRAME_SIZE: WZP_FRAME_SIZE,
  detectVariant: wzpDetectVariant,
  getRoom: wzpGetRoom,
  updateStatus: wzpUpdateStatus,
  updateStats: wzpUpdateStats,
  updateLevel: wzpUpdateLevel,
  startAudioContext: wzpStartAudioContext,
  getAudioContext: wzpGetAudioContext,
  connectCapture: wzpConnectCapture,
  connectPlayback: wzpConnectPlayback,
  initUI: wzpInitUI,
};
