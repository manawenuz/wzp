import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { createIdenticonEl } from "./identicon";

// ── Ringer (reused from original) ─────────────────────────────────
class Ringer {
  private ctx: AudioContext | null = null;
  private timer: number | null = null;
  private activeNodes: AudioNode[] = [];
  private running = false;
  start() {
    if (this.running) return;
    this.running = true;
    try {
      if (!this.ctx) this.ctx = new (window.AudioContext || (window as any).webkitAudioContext)();
    } catch { this.running = false; return; }
    this.playOnce();
    this.timer = window.setInterval(() => this.playOnce(), 6000);
  }
  stop() {
    this.running = false;
    if (this.timer != null) { window.clearInterval(this.timer); this.timer = null; }
    for (const n of this.activeNodes) try { (n as any).disconnect(); } catch {}
    this.activeNodes = [];
  }
  private playOnce() {
    if (!this.ctx || !this.running) return;
    const ctx = this.ctx;
    const gain = ctx.createGain();
    gain.gain.setValueAtTime(0, ctx.currentTime);
    gain.gain.linearRampToValueAtTime(0.3, ctx.currentTime + 0.05);
    gain.gain.setValueAtTime(0.3, ctx.currentTime + 1.95);
    gain.gain.linearRampToValueAtTime(0, ctx.currentTime + 2.0);
    gain.connect(ctx.destination);
    for (const freq of [440, 480]) {
      const osc = ctx.createOscillator();
      osc.frequency.value = freq;
      osc.connect(gain);
      osc.start(ctx.currentTime);
      osc.stop(ctx.currentTime + 2.0);
      this.activeNodes.push(osc);
    }
    this.activeNodes.push(gain);
  }
}
const ringer = new Ringer();

// ── Disable zoom/rubber-banding ───────────────────────────────────
document.addEventListener("touchmove", (e) => { if ((e as any).scale !== undefined && (e as any).scale !== 1) e.preventDefault(); }, { passive: false });
document.addEventListener("gesturestart", (e) => e.preventDefault());
document.addEventListener("gesturechange", (e) => e.preventDefault());
document.addEventListener("wheel", (e) => { if (e.ctrlKey) e.preventDefault(); }, { passive: false });

// ── Elements ──────────────────────────────────────────────────────
const lobbyScreen = document.getElementById("lobby-screen")!;
const callScreen = document.getElementById("call-screen")!;
const lobbyDot = document.getElementById("lobby-dot")!;
const lobbyRelayLabel = document.getElementById("lobby-relay-label")!;
const lobbyRoomLabel = document.getElementById("lobby-room-label")!;
const lobbyIdenticon = document.getElementById("lobby-identicon")!;
const lobbyFp = document.getElementById("lobby-fp")!;
const lobbyUserList = document.getElementById("lobby-user-list")!;
const lobbyUserCount = document.getElementById("lobby-user-count")!;
const joinVoiceBtn = document.getElementById("join-voice-btn")!;
const incomingBanner = document.getElementById("incoming-call-banner")!;
const incomingCallerName = document.getElementById("incoming-caller-name")!;
const incomingIdenticon = document.getElementById("incoming-identicon")!;
const acceptCallBtn = document.getElementById("accept-call-btn")!;
const rejectCallBtn = document.getElementById("reject-call-btn")!;
// Voice drawer elements
const voiceDrawer = document.getElementById("voice-drawer")!;
const vdRoom = document.getElementById("vd-room")!;
const vdTimer = document.getElementById("vd-timer")!;
const vdStatus = document.getElementById("vd-status")!;
const vdBadge = document.getElementById("vd-badge")!;
const vdLevelBar = document.getElementById("vd-level-bar")!;
const vdMicBtn = document.getElementById("vd-mic-btn")!;
const vdMicIcon = document.getElementById("vd-mic-icon")!;
const vdSpkBtn = document.getElementById("vd-spk-btn")!;
const vdSpkIcon = document.getElementById("vd-spk-icon")!;
const vdEndBtn = document.getElementById("vd-end-btn")!;
const vdCamBtn = document.getElementById("vd-cam-btn")!;
const vdCamIcon = document.getElementById("vd-cam-icon")!;
const vdVideoStrip = document.getElementById("vd-video-strip")!;
const vdRemoteVideo = document.getElementById("vd-remote-video") as HTMLCanvasElement;
const vdLocalVideo = document.getElementById("vd-local-video") as HTMLVideoElement;
const vdDirectInfo = document.getElementById("vd-direct-info")!;
const vdDcIdenticon = document.getElementById("vd-dc-identicon")!;
const vdDcName = document.getElementById("vd-dc-name")!;
const vdDcBadge = document.getElementById("vd-dc-badge")!;
const vdStats = document.getElementById("vd-stats")!;
const ctxMenu = document.getElementById("user-context-menu")!;
const ctxIdenticon = document.getElementById("ctx-identicon")!;
const ctxName = document.getElementById("ctx-name")!;
const ctxFp = document.getElementById("ctx-fp")!;
const ctxCallBtn = document.getElementById("ctx-call-btn")!;
const ctxCloseBtn = document.getElementById("ctx-close-btn")!;
// Relay management
const sRelayList = document.getElementById("s-relay-list")!;
const sRelayName = document.getElementById("s-relay-name") as HTMLInputElement;
const sRelayAddr = document.getElementById("s-relay-addr") as HTMLInputElement;
const sRelayAdd = document.getElementById("s-relay-add")!;

// Settings
const settingsPanel = document.getElementById("settings-panel")!;
const settingsBtn = document.getElementById("settings-btn")!;
const settingsBtnCall = document.getElementById("settings-btn-call")!;
const settingsClose = document.getElementById("settings-close")!;
const settingsSave = document.getElementById("settings-save")!;
const sRoom = document.getElementById("s-room") as HTMLInputElement;
const sAlias = document.getElementById("s-alias") as HTMLInputElement;
const sOsAec = document.getElementById("s-os-aec") as HTMLInputElement;
const sDredDebug = document.getElementById("s-dred-debug") as HTMLInputElement;
const sCallDebug = document.getElementById("s-call-debug") as HTMLInputElement;
const sDirectOnly = document.getElementById("s-direct-only") as HTMLInputElement;
const sBirthdayAttack = document.getElementById("s-birthday-attack") as HTMLInputElement;
const sCallDebugSection = document.getElementById("s-call-debug-section") as HTMLDivElement;
const sCallDebugLogEl = document.getElementById("s-call-debug-log") as HTMLDivElement;
const sCallDebugClearBtn = document.getElementById("s-call-debug-clear") as HTMLButtonElement;
const sCallDebugCopyBtn = document.getElementById("s-call-debug-copy") as HTMLButtonElement;
const sCallDebugShareBtn = document.getElementById("s-call-debug-share") as HTMLButtonElement;
const sQuality = document.getElementById("s-quality") as HTMLInputElement;
const sQualityLabel = document.getElementById("s-quality-label")!;
const sFingerprint = document.getElementById("s-fingerprint")!;
const sPublicAddr = document.getElementById("s-public-addr")!;
const sReflectBtn = document.getElementById("s-reflect-btn")!;
const sNatDetectBtn = document.getElementById("s-nat-detect-btn")!;
const sNatResult = document.getElementById("s-nat-result")!;

// ── State ─────────────────────────────────────────────────────────
interface RelayServer { name: string; address: string; }
interface RecentRoom { relay: string; room: string; }
interface Settings {
  relays: RelayServer[];
  selectedRelay: number;
  room: string;
  alias: string;
  osAec: boolean;
  quality: string;
  recentRooms: RecentRoom[];
  dredDebugLogs: boolean;
  callDebugLogs: boolean;
  directOnly: boolean;
  birthdayAttack: boolean;
}

function loadSettings(): Settings {
  const defaults: Settings = {
    relays: [
      { name: "Default", address: "193.180.213.68:4433" },
    ],
    selectedRelay: 0, room: "general", alias: "",
    osAec: true, quality: "auto", recentRooms: [],
    dredDebugLogs: false, callDebugLogs: false,
    directOnly: false, birthdayAttack: false,
  };
  try {
    const raw = localStorage.getItem("wzp-settings");
    if (raw) return { ...defaults, ...JSON.parse(raw) };
  } catch {}
  return defaults;
}
function saveSettings(s: Settings) {
  localStorage.setItem("wzp-settings", JSON.stringify(s));
}
function getRelay(): RelayServer | null {
  const s = loadSettings();
  return s.relays[s.selectedRelay] || s.relays[0] || null;
}

let myFingerprint = "";
let statusInterval: number | null = null;
let inVoice = false;
let connectPending = false; // guard against double-tap while connect is in-flight
let directCallPeer: { fingerprint: string; alias: string | null } | null = null;
let pendingCallId: string | null = null;

// Video / camera state
let cameraActive = false;
let cameraStream: MediaStream | null = null;
let cameraFrameTimer: number | null = null;
let remoteVideoActive = false;

function showToast(msg: string, durationMs = 3500) {
  let el = document.getElementById("wzp-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "wzp-toast";
    el.style.cssText = "position:fixed;bottom:80px;left:50%;transform:translateX(-50%);" +
      "background:#1e1e2e;color:#cdd6f4;border:1px solid #45475a;border-radius:8px;" +
      "padding:10px 18px;font-size:13px;z-index:9999;pointer-events:none;opacity:0;transition:opacity .2s";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.style.opacity = "1";
  clearTimeout((el as any)._timer);
  (el as any)._timer = setTimeout(() => { el!.style.opacity = "0"; }, durationMs);
}

function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) {
    const msg = (e as { message?: unknown }).message;
    if (typeof msg === "string") return msg;
  }
  return String(e);
}

function connectDebugSummary(entry: CallDebugEntry | null): string {
  if (!entry) return "no native connect event received";
  const details = entry.details && typeof entry.details === "object"
    ? JSON.stringify(entry.details)
    : String(entry.details ?? "");
  return `${entry.step}${details ? ` ${details}` : ""}`;
}

let lastConnectDebug: CallDebugEntry | null = null;

function connectWithTimeout(args: Record<string, unknown>, timeoutMs = 45000) {
  lastConnectDebug = null;
  return Promise.race([
    invoke("connect", args),
    new Promise<never>((_, reject) =>
      setTimeout(() => reject(new Error(
        `connect timed out (${Math.round(timeoutMs / 1000)}s); last native step: ${connectDebugSummary(lastConnectDebug)}`
      )), timeoutMs)
    ),
  ]);
}

// Known users in the room (from RoomUpdate or signal presence)
interface LobbyUser {
  fingerprint: string;
  alias: string | null;
  inVoice: boolean;
  speaking: boolean;
}
let lobbyUsers: Map<string, LobbyUser> = new Map();

// ── Call debug buffer ─────────────────────────────────────────────
interface CallDebugEntry { ts_ms: number; step: string; details: any; }
const callDebugBuffer: CallDebugEntry[] = [];
const CALL_DEBUG_MAX = 200;

listen("call-debug-log", (event: any) => {
  const entry: CallDebugEntry = event.payload;
  callDebugBuffer.push(entry);
  if (entry.step?.startsWith("connect:")) lastConnectDebug = entry;
  if (callDebugBuffer.length > CALL_DEBUG_MAX) callDebugBuffer.shift();
  renderCallDebugLog();
});

function renderCallDebugLog() {
  if (!sCallDebugLogEl) return;
  sCallDebugLogEl.textContent = callDebugBuffer
    .map((e) => {
      const t = new Date(e.ts_ms).toLocaleTimeString("en-GB", { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit", fractionalSecondDigits: 3 } as any);
      const d = typeof e.details === "object" ? JSON.stringify(e.details) : String(e.details);
      return `${t} ${e.step} ${d}`;
    })
    .join("\n");
  sCallDebugLogEl.scrollTop = sCallDebugLogEl.scrollHeight;
}

// ── Quality slider ────────────────────────────────────────────────
const QUALITY_STEPS = ["studio-64k", "studio-48k", "studio-32k", "auto", "good", "degraded", "codec2-3200", "catastrophic"];
const QUALITY_LABELS = ["Studio 64k", "Studio 48k", "Studio 32k", "Auto", "Opus 24k", "Opus 6k", "Codec2 3.2k", "Codec2 1.2k"];
const QUALITY_COLORS = ["#22c55e", "#4ade80", "#86efac", "#a3e635", "#facc15", "#f59e0b", "#e97320", "#991b1b"];

function qualityToIndex(q: string): number { const i = QUALITY_STEPS.indexOf(q); return i >= 0 ? i : 3; }
function updateQualityUI(i: number) {
  if (sQualityLabel) { sQualityLabel.textContent = QUALITY_LABELS[i]; sQualityLabel.style.color = QUALITY_COLORS[i]; }
}
sQuality?.addEventListener("input", () => updateQualityUI(parseInt(sQuality.value)));

// ── Lobby rendering ───────────────────────────────────────────────
function renderLobbyUsers() {
  lobbyUserList.innerHTML = "";
  const users = Array.from(lobbyUsers.values())
    .filter((u) => u.fingerprint !== myFingerprint) // always exclude self
    .sort((a, b) => {
    // Voice users first, then alphabetical
    if (a.inVoice !== b.inVoice) return a.inVoice ? -1 : 1;
    return (a.alias || a.fingerprint).localeCompare(b.alias || b.fingerprint);
  });

  lobbyUserCount.textContent = String(users.length);

  if (users.length === 0) {
    lobbyUserList.innerHTML = '<div class="lobby-empty">No one else is here yet</div>';
    return;
  }

  for (const user of users) {
    const row = document.createElement("div");
    row.className = "user-row" + (user.inVoice ? " in-voice" : "") + (user.speaking ? " speaking" : "");
    row.dataset.fp = user.fingerprint;

    const identicon = document.createElement("div");
    identicon.className = "user-identicon";
    identicon.appendChild(createIdenticonEl(user.fingerprint, 36, true));

    const info = document.createElement("div");
    info.className = "user-info";
    info.innerHTML = `<div class="user-name">${user.alias || user.fingerprint.substring(0, 16)}</div>
      <div class="user-fp">${user.fingerprint}</div>`;

    const status = document.createElement("div");
    status.className = "user-status";
    if (user.speaking) {
      status.innerHTML = '<span class="user-status-icon">&#x1F50A;</span>';
    } else if (user.inVoice) {
      status.innerHTML = '<span class="user-status-icon">&#x1F3A7;</span>';
    }

    row.appendChild(identicon);
    row.appendChild(info);
    row.appendChild(status);

    row.addEventListener("click", () => openContextMenu(user));
    lobbyUserList.appendChild(row);
  }
}

// ── Context menu ──────────────────────────────────────────────────
let contextUser: LobbyUser | null = null;

function openContextMenu(user: LobbyUser) {
  contextUser = user;
  ctxIdenticon.innerHTML = "";
  ctxIdenticon.appendChild(createIdenticonEl(user.fingerprint, 40, true));
  ctxName.textContent = user.alias || user.fingerprint.substring(0, 16);
  ctxFp.textContent = user.fingerprint;
  // Hide call button for self
  const isSelf = user.fingerprint === myFingerprint;
  (ctxCallBtn as HTMLButtonElement).disabled = isSelf;
  (ctxCallBtn as HTMLElement).style.opacity = isSelf ? "0.3" : "1";
  ctxMenu.classList.remove("hidden");
}

ctxCloseBtn.addEventListener("click", () => ctxMenu.classList.add("hidden"));
ctxMenu.addEventListener("click", (e) => { if (e.target === ctxMenu) ctxMenu.classList.add("hidden"); });

let callInProgress = false;
ctxCallBtn.addEventListener("click", async () => {
  if (!contextUser || callInProgress) return;
  if (contextUser.fingerprint === myFingerprint) {
    ctxMenu.classList.add("hidden");
    return;
  }
  // Don't place a call if there's already a pending incoming call
  if (pendingCallId) {
    ctxMenu.classList.add("hidden");
    return;
  }
  callInProgress = true;
  ctxMenu.classList.add("hidden");
  directCallPeer = { fingerprint: contextUser.fingerprint, alias: contextUser.alias };
  try {
    await invoke("place_call", { targetFp: contextUser.fingerprint });
    // Keep callInProgress true until the call resolves (setup/hangup)
    // — it's cleared in leaveVoice() or when the call connects
  } catch (e: any) {
    console.error("place_call failed:", e);
    directCallPeer = null;
    callInProgress = false;
  }
});

// ── Voice join/leave (drawer-based) ───────────────────────────────
joinVoiceBtn.addEventListener("click", async () => {
  if (inVoice || connectPending) return;
  const relay = getRelay();
  const s = loadSettings();
  if (!relay) { showToast("No relay configured"); return; }
  connectPending = true;
  const origText = joinVoiceBtn.textContent;
  joinVoiceBtn.textContent = "Connecting…";
  (joinVoiceBtn as HTMLButtonElement).disabled = true;
  try {
    await connectWithTimeout({
      relay: relay.address,
      room: s.room || "general",
      alias: s.alias || "",
      osAec: s.osAec,
      quality: s.quality || "auto",
    });
    enterVoice(false);
  } catch (e: any) {
    console.error("connect failed:", e);
    showToast(`Join failed: ${errorMessage(e)}`);
  } finally {
    connectPending = false;
    joinVoiceBtn.textContent = origText;
    (joinVoiceBtn as HTMLButtonElement).disabled = false;
  }
});

function enterVoice(isDirect: boolean) {
  inVoice = true;
  const s = loadSettings();
  joinVoiceBtn.classList.add("hidden");
  voiceDrawer.classList.remove("hidden");
  vdRoom.textContent = isDirect && directCallPeer
    ? (directCallPeer.alias || directCallPeer.fingerprint.substring(0, 16))
    : (s.room || "general");
  vdTimer.textContent = "0:00";
  vdBadge.classList.add("hidden");
  vdBadge.textContent = "";

  if (isDirect && directCallPeer) {
    vdDirectInfo.classList.remove("hidden");
    vdDcIdenticon.innerHTML = "";
    vdDcIdenticon.appendChild(createIdenticonEl(directCallPeer.fingerprint, 32, true));
    vdDcName.textContent = directCallPeer.alias || directCallPeer.fingerprint.substring(0, 16);
    vdDcBadge.textContent = "Connecting...";
    vdDcBadge.className = "vd-dc-badge connecting";
  } else {
    vdDirectInfo.classList.add("hidden");
  }

  statusInterval = window.setInterval(pollStatus, 250);
}

function leaveVoice() {
  inVoice = false;
  callInProgress = false;
  directCallPeer = null;
  pendingCallId = null;
  voiceDrawer.classList.add("hidden");
  joinVoiceBtn.classList.remove("hidden");
  vdLevelBar.style.width = "0%";
  if (statusInterval) { clearInterval(statusInterval); statusInterval = null; }
  stopCamera();
  remoteVideoActive = false;
  vdVideoStrip.classList.add("hidden");
  remoteCtx.clearRect(0, 0, vdRemoteVideo.width, vdRemoteVideo.height);
}

// Drawer controls
vdEndBtn.addEventListener("click", async () => {
  try { await invoke("hangup_call"); } catch {}
  try { await invoke("disconnect"); } catch {}
  leaveVoice();
});
vdMicBtn.addEventListener("click", async () => {
  try { await invoke("toggle_mic"); } catch {}
});
vdSpkBtn.addEventListener("click", async () => {
  try { await invoke("toggle_speaker"); } catch {}
});

// ── Camera (Blocker 4 + 5) ────────────────────────────────────────
const camCaptureCanvas = document.createElement("canvas");
const camCaptureCtx = camCaptureCanvas.getContext("2d")!;

async function startCamera() {
  if (cameraActive) return;
  try {
    cameraStream = await navigator.mediaDevices.getUserMedia({
      video: { width: { ideal: 1280 }, height: { ideal: 720 }, facingMode: "user" },
      audio: false,
    });
    vdLocalVideo.srcObject = cameraStream;
    vdVideoStrip.classList.remove("hidden");

    const track = cameraStream.getVideoTracks()[0];
    const settings = track.getSettings();
    camCaptureCanvas.width = settings.width ?? 640;
    camCaptureCanvas.height = settings.height ?? 360;

    cameraActive = true;
    vdCamIcon.textContent = "Cam ✓";
    vdCamBtn.classList.add("active");

    // Capture loop at ~15 fps
    cameraFrameTimer = window.setInterval(async () => {
      if (!cameraActive) return;
      camCaptureCtx.drawImage(vdLocalVideo, 0, 0, camCaptureCanvas.width, camCaptureCanvas.height);
      const dataUrl = camCaptureCanvas.toDataURL("image/jpeg", 0.75);
      const b64 = dataUrl.slice(dataUrl.indexOf(",") + 1);
      try { await invoke("push_camera_frame", { jpeg_b64: b64 }); } catch { /* call not active */ }
    }, 67); // 67 ms ≈ 15 fps
  } catch (e) {
    console.warn("camera access denied or unavailable:", e);
  }
}

function stopCamera() {
  cameraActive = false;
  if (cameraFrameTimer != null) { window.clearInterval(cameraFrameTimer); cameraFrameTimer = null; }
  if (cameraStream) { cameraStream.getTracks().forEach(t => t.stop()); cameraStream = null; }
  vdLocalVideo.srcObject = null;
  vdCamIcon.textContent = "Cam";
  vdCamBtn.classList.remove("active");
  // Hide strip only if remote video is also gone
  if (!remoteVideoActive) vdVideoStrip.classList.add("hidden");
}

vdCamBtn.addEventListener("click", () => {
  if (cameraActive) { stopCamera(); } else { startCamera(); }
});

// ── Remote video display (Blocker 5) ─────────────────────────────
const remoteCtx = vdRemoteVideo.getContext("2d")!;

listen("video:frame", (event: any) => {
  const { width, height, jpeg_b64 } = event.payload;
  if (!jpeg_b64) return;

  remoteVideoActive = true;
  vdVideoStrip.classList.remove("hidden");
  vdRemoteVideo.width = width ?? vdRemoteVideo.width;
  vdRemoteVideo.height = height ?? vdRemoteVideo.height;

  const img = new Image();
  img.onload = () => {
    remoteCtx.drawImage(img, 0, 0, vdRemoteVideo.width, vdRemoteVideo.height);
  };
  img.src = `data:image/jpeg;base64,${jpeg_b64}`;
});

// ── Poll status ───────────────────────────────────────────────────
interface CallStatusI {
  active: boolean;
  mic_muted: boolean;
  spk_muted: boolean;
  participants: any[];
  encode_fps: number;
  recv_fps: number;
  audio_level: number;
  call_duration_secs: number;
  fingerprint: string;
  tx_codec: string;
  rx_codec: string;
}

async function pollStatus() {
  try {
    const st: CallStatusI = await invoke("get_status");
    if (!st.active) {
      leaveVoice();
      return;
    }
    if (st.fingerprint) myFingerprint = st.fingerprint;

    // Update drawer controls
    vdMicBtn.classList.toggle("muted", st.mic_muted);
    vdMicIcon.textContent = st.mic_muted ? "Muted" : "Mic";
    vdSpkBtn.classList.toggle("muted", st.spk_muted);
    vdSpkIcon.textContent = st.spk_muted ? "Off" : "Spk";

    // Level meter
    const pct = Math.min(100, (st.audio_level / 10000) * 100);
    vdLevelBar.style.width = `${pct}%`;

    // Duration
    const m = Math.floor(st.call_duration_secs / 60);
    const s = Math.floor(st.call_duration_secs % 60);
    vdTimer.textContent = `${m}:${s.toString().padStart(2, "0")}`;

    // P2P badge for direct calls
    if (directCallPeer) {
      const pathNeg = [...callDebugBuffer].reverse().find((e) => e.step === "connect:path_negotiated");
      const engineOk = [...callDebugBuffer].reverse().find((e) => e.step === "connect:call_engine_started");
      if (engineOk) {
        if (pathNeg?.details?.use_direct === true) {
          vdDcBadge.textContent = "P2P Direct";
          vdDcBadge.className = "vd-dc-badge direct";
          vdBadge.textContent = "P2P";
          vdBadge.className = "vd-badge direct";
          vdBadge.classList.remove("hidden");
        } else {
          vdDcBadge.textContent = "Via Relay";
          vdDcBadge.className = "vd-dc-badge relay";
          vdBadge.textContent = "Relay";
          vdBadge.className = "vd-badge relay";
          vdBadge.classList.remove("hidden");
        }
      }
    }

    // Stats with codec
    vdStats.textContent = `TX: ${st.tx_codec || "?"} ${st.encode_fps || 0}fps | RX: ${st.rx_codec || "?"} ${st.recv_fps || 0}fps | Level: ${st.audio_level || 0}`;
  } catch {}
}

// ── Signal events ─────────────────────────────────────────────────
listen("signal-event", (event: any) => {
  const data = event.payload;
  switch (data.type) {
    case "presence_list":
      // Relay sent updated user list
      lobbyUsers.clear();
      for (const u of data.users || []) {
        if (u.fingerprint === myFingerprint) continue; // don't show self
        lobbyUsers.set(u.fingerprint, {
          fingerprint: u.fingerprint,
          alias: u.alias || null,
          inVoice: false,
          speaking: false,
        });
      }
      renderLobbyUsers();
      break;
    case "ringing":
      // We placed a call, it's ringing
      break;
    case "incoming":
      // Show incoming call banner
      incomingBanner.classList.remove("hidden");
      incomingCallerName.textContent = data.caller_alias || data.caller_fp?.substring(0, 16) || "Unknown";
      incomingIdenticon.innerHTML = "";
      incomingIdenticon.appendChild(createIdenticonEl(data.caller_fp || "?", 40, true));
      directCallPeer = { fingerprint: data.caller_fp || "", alias: data.caller_alias || null };
      pendingCallId = data.call_id || null;
      ringer.start();
      break;
    case "answered":
      ringer.stop();
      break;
    case "setup":
      ringer.stop();
      incomingBanner.classList.add("hidden");
      // Auto-connect to the call
      (async () => {
        if (connectPending) return;
        connectPending = true;
        const s = loadSettings();
        try {
          await connectWithTimeout({
            relay: data.relay_addr,
            room: data.room,
            alias: s.alias || "",
            osAec: s.osAec,
            quality: s.quality || "auto",
            peerDirectAddr: data.peer_direct_addr ?? null,
            peerLocalAddrs: data.peer_local_addrs ?? [],
            peerMappedAddr: data.peer_mapped_addr ?? null,
            directOnly: s.directOnly || false,
            birthdayAttack: s.birthdayAttack || false,
          });
          enterVoice(true);
        } catch (e: any) {
          console.error("connect failed:", e);
          showToast(`Call failed to connect: ${errorMessage(e)}`);
        } finally {
          connectPending = false;
        }
      })();
      break;
    case "hangup":
      ringer.stop();
      incomingBanner.classList.add("hidden");
      (async () => {
        try { await invoke("disconnect"); } catch {}
        leaveVoice();
      })();
      break;
  }
});

// Accept/reject incoming call
acceptCallBtn.addEventListener("click", async () => {
  ringer.stop();
  incomingBanner.classList.add("hidden");
  if (pendingCallId) {
    await invoke("answer_call", { callId: pendingCallId, mode: 1 });
    pendingCallId = null;
  }
});

rejectCallBtn.addEventListener("click", async () => {
  ringer.stop();
  incomingBanner.classList.add("hidden");
  if (pendingCallId) {
    await invoke("answer_call", { callId: pendingCallId, mode: 0 });
    pendingCallId = null;
    directCallPeer = null;
  }
});

// ── Room updates (participants) ───────────────────────────────────
listen("call-event", (event: any) => {
  const data = event.payload;
  if (data.kind === "participants" && data.participants) {
    // Update lobby users from room participant list
    const active = new Set<string>();
    for (const p of data.participants) {
      const fp = p.fingerprint || p.id || "";
      active.add(fp);
      if (!lobbyUsers.has(fp)) {
        lobbyUsers.set(fp, { fingerprint: fp, alias: p.alias || null, inVoice: true, speaking: false });
      } else {
        const u = lobbyUsers.get(fp)!;
        u.inVoice = true;
        if (p.alias) u.alias = p.alias;
      }
    }
    // Mark users not in participant list as not in voice
    for (const [fp, u] of lobbyUsers) {
      if (!active.has(fp)) u.inVoice = false;
    }
    renderLobbyUsers();
  }
});

// ── Settings ──────────────────────────────────────────────────────
// ── Relay list management ──────────────────────────────────────
function renderRelayList() {
  const s = loadSettings();
  sRelayList.innerHTML = "";
  for (let i = 0; i < s.relays.length; i++) {
    const r = s.relays[i];
    const isActive = i === s.selectedRelay;
    const row = document.createElement("div");
    row.style.cssText = "display:flex;align-items:center;gap:6px;padding:8px;border-radius:6px;margin-bottom:4px;cursor:pointer;" +
      (isActive ? "background:rgba(74,222,128,0.12);border:1px solid var(--green);" : "background:var(--surface);border:1px solid transparent;");
    row.innerHTML = `
      <span style="flex:1;font-size:13px;font-weight:${isActive ? '600' : '400'}">
        <span style="color:${isActive ? 'var(--green)' : 'var(--text)'}">${r.name}</span>
        <span style="color:var(--text-dim);font-size:11px;margin-left:4px">${r.address}</span>
      </span>
      ${isActive ? '<span style="color:var(--green);font-size:11px">ACTIVE</span>' : ''}
      <button class="relay-rm-btn" data-idx="${i}" style="background:none;border:none;color:var(--text-dim);cursor:pointer;font-size:16px;padding:2px 6px">&times;</button>
    `;
    // Click to select (not on the X button)
    row.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).classList.contains("relay-rm-btn")) return;
      const settings = loadSettings();
      if (i !== settings.selectedRelay) {
        settings.selectedRelay = i;
        saveSettings(settings);
        renderRelayList();
        // Reconnect to new relay
        reconnectSignal();
      }
    });
    sRelayList.appendChild(row);
  }
  // Wire remove buttons
  sRelayList.querySelectorAll(".relay-rm-btn").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const idx = parseInt((btn as HTMLElement).dataset.idx || "0");
      const settings = loadSettings();
      if (settings.relays.length <= 1) return; // keep at least one
      settings.relays.splice(idx, 1);
      if (settings.selectedRelay >= settings.relays.length) {
        settings.selectedRelay = 0;
      }
      saveSettings(settings);
      renderRelayList();
      reconnectSignal();
    });
  });
}

sRelayAdd.addEventListener("click", () => {
  const name = sRelayName.value.trim();
  const addr = sRelayAddr.value.trim();
  if (!name || !addr) return;
  if (!addr.includes(":")) return; // must be host:port
  const s = loadSettings();
  s.relays.push({ name, address: addr });
  saveSettings(s);
  sRelayName.value = "";
  sRelayAddr.value = "";
  renderRelayList();
});

async function reconnectSignal() {
  // Deregister from current relay, then auto-connect to new one
  try { await invoke("deregister"); } catch {}
  lobbyUsers.clear();
  renderLobbyUsers();
  lobbyDot.style.background = "var(--yellow)";
  lobbyRelayLabel.textContent = "Reconnecting...";
  // Short delay to let deregister complete
  setTimeout(() => autoConnect(), 500);
}

function openSettings() {
  const s = loadSettings();
  sRoom.value = s.room;
  sAlias.value = s.alias;
  sOsAec.checked = s.osAec;
  sDredDebug.checked = !!s.dredDebugLogs;
  sCallDebug.checked = !!s.callDebugLogs;
  sDirectOnly.checked = !!s.directOnly;
  sBirthdayAttack.checked = !!s.birthdayAttack;
  sCallDebugSection.style.display = s.callDebugLogs ? "" : "none";
  renderCallDebugLog();
  const qi = qualityToIndex(s.quality || "auto");
  sQuality.value = String(qi);
  updateQualityUI(qi);
  sFingerprint.textContent = myFingerprint || "(loading...)";
  renderRelayList();
  settingsPanel.classList.remove("hidden");
}

settingsBtn.addEventListener("click", openSettings);
settingsBtnCall?.addEventListener("click", openSettings);
settingsClose.addEventListener("click", () => settingsPanel.classList.add("hidden"));
settingsPanel.addEventListener("click", (e) => { if (e.target === settingsPanel) settingsPanel.classList.add("hidden"); });

settingsSave.addEventListener("click", () => {
  const s = loadSettings();
  s.room = sRoom.value;
  s.alias = sAlias.value;
  s.osAec = sOsAec.checked;
  s.quality = QUALITY_STEPS[parseInt(sQuality.value)] || "auto";
  s.dredDebugLogs = sDredDebug.checked;
  s.callDebugLogs = sCallDebug.checked;
  s.directOnly = sDirectOnly.checked;
  s.birthdayAttack = sBirthdayAttack.checked;
  saveSettings(s);
  invoke("set_dred_verbose_logs", { enabled: s.dredDebugLogs }).catch(() => {});
  invoke("set_call_debug_logs", { enabled: s.callDebugLogs }).catch(() => {});
  sCallDebugSection.style.display = s.callDebugLogs ? "" : "none";
  // Update lobby room label
  lobbyRoomLabel.textContent = s.room || "general";
  settingsPanel.classList.add("hidden");
});

// Debug log actions
sCallDebugClearBtn?.addEventListener("click", () => {
  callDebugBuffer.length = 0;
  sCallDebugLogEl.textContent = "";
});
sCallDebugCopyBtn?.addEventListener("click", () => {
  const text = callDebugBuffer.map((e) => {
    const t = new Date(e.ts_ms).toLocaleTimeString("en-GB", { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" });
    return `${t} ${e.step} ${JSON.stringify(e.details)}`;
  }).join("\n");
  navigator.clipboard?.writeText(text).catch(() => {});
});
sCallDebugShareBtn?.addEventListener("click", async () => {
  const text = callDebugBuffer.map((e) => {
    const t = new Date(e.ts_ms).toLocaleTimeString("en-GB", { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" });
    return `${t} ${e.step} ${JSON.stringify(e.details)}`;
  }).join("\n");
  try { await (navigator as any).share({ text }); } catch {}
});

// NAT detect
sReflectBtn?.addEventListener("click", async () => {
  try {
    const addr: string = await invoke("get_reflected_address");
    sPublicAddr.textContent = addr;
  } catch (e: any) {
    sPublicAddr.textContent = String(e);
  }
});

sNatDetectBtn?.addEventListener("click", async () => {
  sNatResult.textContent = "Detecting...";
  try {
    const relay = getRelay();
    const relays = relay ? [{ name: relay.name, address: relay.address }] : [];
    const result: any = await invoke("detect_nat_type", { relays });
    let text = `NAT: ${result.nat_type}`;
    if (result.consensus_addr) text += ` (${result.consensus_addr})`;
    text += "\n";
    for (const p of result.probes || []) {
      text += `  ${p.relay_name} (${p.relay_addr}) → ${p.observed_addr || "failed"} [${p.latency_ms || "-"}ms]`;
      if (p.error) text += ` [${p.error}]`;
      text += "\n";
    }
    sNatResult.textContent = text;
  } catch (e: any) {
    sNatResult.textContent = String(e);
  }
});

// ── Auto-connect signal on launch ─────────────────────────────────
async function autoConnect() {
  const relay = getRelay();
  const s = loadSettings();
  if (!relay) {
    lobbyRelayLabel.textContent = "No relay configured";
    lobbyDot.style.background = "var(--red)";
    return;
  }

  lobbyRelayLabel.textContent = `${relay.name} (${relay.address})`;
  lobbyRoomLabel.textContent = s.room || "general";
  lobbyDot.style.background = "var(--yellow)";

  try {
    // Register signal for presence + direct calls
    await invoke("register_signal", { relay: relay.address });
    lobbyDot.style.background = "var(--green)";
    lobbyRelayLabel.textContent = `${relay.name} — connected`;

    // Get identity + alias
    const appInfo: any = await invoke("get_app_info");
    if (appInfo?.fingerprint) {
      myFingerprint = appInfo.fingerprint;
      lobbyFp.textContent = appInfo.alias || appInfo.fingerprint;
      lobbyIdenticon.innerHTML = "";
      lobbyIdenticon.appendChild(createIdenticonEl(appInfo.fingerprint, 20, true));
    }
  } catch (e: any) {
    lobbyDot.style.background = "var(--red)";
    lobbyRelayLabel.textContent = `Failed: ${e}`;
  }
}

// Push debug log setting to Rust on startup
invoke("set_call_debug_logs", { enabled: !!loadSettings().callDebugLogs }).catch(() => {});

// Keyboard shortcuts
document.addEventListener("keydown", (e) => {
  if ((e.target as HTMLElement).tagName === "INPUT") return;
  if (e.key === "m") vdMicBtn.click();
  if (e.key === "q") vdEndBtn.click();
  if (e.key === "s") vdSpkBtn.click();
  if (e.key === "v") vdCamBtn.click();
  if (e.key === "," && (e.metaKey || e.ctrlKey)) { e.preventDefault(); openSettings(); }
});

// Launch
autoConnect();
