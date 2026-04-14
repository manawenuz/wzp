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
const backToLobbyBtn = document.getElementById("back-to-lobby-btn")!;
const roomName = document.getElementById("room-name")!;
const callTimer = document.getElementById("call-timer")!;
const callStatus = document.getElementById("call-status")!;
const levelBar = document.getElementById("level-bar")!;
const participantsDiv = document.getElementById("participants")!;
const directCallView = document.getElementById("direct-call-view")!;
const dcIdenticon = document.getElementById("dc-identicon")!;
const dcName = document.getElementById("dc-name")!;
const dcFp = document.getElementById("dc-fp")!;
const dcBadge = document.getElementById("dc-badge")!;
const micBtn = document.getElementById("mic-btn")!;
const micIcon = document.getElementById("mic-icon")!;
const spkBtn = document.getElementById("spk-btn")!;
const spkIcon = document.getElementById("spk-icon")!;
const hangupBtn = document.getElementById("hangup-btn")!;
const statsDiv = document.getElementById("stats")!;
const ctxMenu = document.getElementById("user-context-menu")!;
const ctxIdenticon = document.getElementById("ctx-identicon")!;
const ctxName = document.getElementById("ctx-name")!;
const ctxFp = document.getElementById("ctx-fp")!;
const ctxCallBtn = document.getElementById("ctx-call-btn")!;
const ctxCloseBtn = document.getElementById("ctx-close-btn")!;
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
let directCallPeer: { fingerprint: string; alias: string | null } | null = null;
let pendingCallId: string | null = null;

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
  const users = Array.from(lobbyUsers.values()).sort((a, b) => {
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
  ctxMenu.classList.remove("hidden");
}

ctxCloseBtn.addEventListener("click", () => ctxMenu.classList.add("hidden"));
ctxMenu.addEventListener("click", (e) => { if (e.target === ctxMenu) ctxMenu.classList.add("hidden"); });

ctxCallBtn.addEventListener("click", async () => {
  if (!contextUser) return;
  ctxMenu.classList.add("hidden");
  directCallPeer = { fingerprint: contextUser.fingerprint, alias: contextUser.alias };
  try {
    await invoke("place_call", { targetFp: contextUser.fingerprint });
  } catch (e: any) {
    console.error("place_call failed:", e);
    directCallPeer = null;
  }
});

// ── Voice join/leave ──────────────────────────────────────────────
joinVoiceBtn.addEventListener("click", async () => {
  if (inVoice) {
    // Leave voice
    try { await invoke("disconnect"); } catch {}
    inVoice = false;
    joinVoiceBtn.innerHTML = '<span class="fab-icon">&#x1F3A7;</span><span class="fab-label">Join Voice</span>';
    joinVoiceBtn.classList.remove("active");
    if (statusInterval) { clearInterval(statusInterval); statusInterval = null; }
    showLobby();
  } else {
    // Join voice
    const relay = getRelay();
    const s = loadSettings();
    if (!relay) return;
    try {
      await invoke("connect", {
        relay: relay.address,
        room: s.room || "general",
        alias: s.alias || "",
        osAec: s.osAec,
        quality: s.quality || "auto",
      });
      inVoice = true;
      joinVoiceBtn.innerHTML = '<span class="fab-icon">&#x1F3A7;</span><span class="fab-label">Leave Voice</span>';
      joinVoiceBtn.classList.add("active");
      showCallScreen(false);
    } catch (e: any) {
      console.error("connect failed:", e);
    }
  }
});

// ── Screen transitions ────────────────────────────────────────────
function showLobby() {
  callScreen.classList.add("hidden");
  lobbyScreen.classList.remove("hidden");
  directCallPeer = null;
  levelBar.style.width = "0%";
}

function showCallScreen(isDirect: boolean) {
  lobbyScreen.classList.add("hidden");
  callScreen.classList.remove("hidden");

  if (isDirect && directCallPeer) {
    roomName.textContent = directCallPeer.alias || directCallPeer.fingerprint.substring(0, 16);
    dcName.textContent = directCallPeer.alias || "Unknown";
    dcFp.textContent = directCallPeer.fingerprint;
    dcIdenticon.innerHTML = "";
    dcIdenticon.appendChild(createIdenticonEl(directCallPeer.fingerprint, 96, true));
    dcBadge.textContent = "Connecting...";
    dcBadge.className = "dc-badge connecting";
    directCallView.classList.remove("hidden");
    participantsDiv.classList.add("hidden");
  } else {
    const s = loadSettings();
    roomName.textContent = s.room || "general";
    directCallView.classList.add("hidden");
    participantsDiv.classList.remove("hidden");
  }
  callStatus.className = "status-dot";
  statusInterval = window.setInterval(pollStatus, 250);
}

// Back button from call to lobby
backToLobbyBtn.addEventListener("click", async () => {
  try { await invoke("disconnect"); } catch {}
  inVoice = false;
  joinVoiceBtn.innerHTML = '<span class="fab-icon">&#x1F3A7;</span><span class="fab-label">Join Voice</span>';
  joinVoiceBtn.classList.remove("active");
  if (statusInterval) { clearInterval(statusInterval); statusInterval = null; }
  showLobby();
});

// Hangup
hangupBtn.addEventListener("click", async () => {
  try { await invoke("hangup_call"); } catch {}
  try { await invoke("disconnect"); } catch {}
  inVoice = false;
  joinVoiceBtn.innerHTML = '<span class="fab-icon">&#x1F3A7;</span><span class="fab-label">Join Voice</span>';
  joinVoiceBtn.classList.remove("active");
  if (statusInterval) { clearInterval(statusInterval); statusInterval = null; }
  showLobby();
});

// Mic/speaker toggles
micBtn.addEventListener("click", async () => {
  try { await invoke("toggle_mic"); } catch {}
});
spkBtn.addEventListener("click", async () => {
  try { await invoke("toggle_speaker"); } catch {}
});

// ── Poll status ───────────────────────────────────────────────────
interface CallStatusI {
  active: boolean;
  mic_muted: boolean;
  speaker_muted: boolean;
  send_rms: number;
  recv_rms: number;
  codec_tx: string;
  codec_rx: string;
  fec_ratio: number;
  send_packets: number;
  recv_packets: number;
  call_duration_secs: number;
  fingerprint: string;
}

async function pollStatus() {
  try {
    const st: CallStatusI = await invoke("get_status");
    if (!st.active) {
      showLobby();
      return;
    }
    if (st.fingerprint) myFingerprint = st.fingerprint;
    micBtn.classList.toggle("muted", st.mic_muted);
    micIcon.textContent = st.mic_muted ? "Mic Off" : "Mic";
    spkBtn.classList.toggle("muted", st.speaker_muted);
    spkIcon.textContent = st.speaker_muted ? "Spk Off" : "Spk";

    const pct = Math.min(100, (st.send_rms / 10000) * 100);
    levelBar.style.width = `${pct}%`;

    // Duration
    const m = Math.floor(st.call_duration_secs / 60);
    const s = Math.floor(st.call_duration_secs % 60);
    callTimer.textContent = `${m}:${s.toString().padStart(2, "0")}`;

    // P2P badge for direct calls
    if (directCallPeer) {
      const pathNeg = [...callDebugBuffer].reverse().find((e) => e.step === "connect:path_negotiated");
      const engineOk = [...callDebugBuffer].reverse().find((e) => e.step === "connect:call_engine_started");
      if (engineOk) {
        if (pathNeg?.details?.use_direct === true) {
          dcBadge.textContent = "P2P Direct";
          dcBadge.className = "dc-badge";
        } else {
          dcBadge.textContent = "Via Relay";
          dcBadge.className = "dc-badge relay";
        }
      }
    }

    statsDiv.textContent = `TX: ${st.codec_tx} ${st.send_packets}pkt | RX: ${st.codec_rx} ${st.recv_packets}pkt | FEC: ${(st.fec_ratio * 100).toFixed(0)}%`;
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
        const s = loadSettings();
        try {
          await invoke("connect", {
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
          showCallScreen(true);
        } catch (e: any) {
          console.error("connect failed:", e);
        }
      })();
      break;
    case "hangup":
      ringer.stop();
      incomingBanner.classList.add("hidden");
      (async () => {
        try { await invoke("disconnect"); } catch {}
        showLobby();
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
  settingsPanel.classList.add("hidden");
});

// Debug log actions
sCallDebugClearBtn?.addEventListener("click", () => {
  callDebugBuffer.length = 0;
  sCallDebugLogEl.textContent = "";
});
sCallDebugCopyBtn?.addEventListener("click", () => {
  const text = callDebugBuffer.map((e) => `${e.step} ${JSON.stringify(e.details)}`).join("\n");
  navigator.clipboard?.writeText(text).catch(() => {});
});
sCallDebugShareBtn?.addEventListener("click", async () => {
  const text = callDebugBuffer.map((e) => `${e.step} ${JSON.stringify(e.details)}`).join("\n");
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

    // Get identity
    const fp: string = await invoke("get_identity");
    if (fp) {
      myFingerprint = fp;
      lobbyFp.textContent = fp;
      lobbyIdenticon.innerHTML = "";
      lobbyIdenticon.appendChild(createIdenticonEl(fp, 20, true));
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
  if (e.key === "m") micBtn.click();
  if (e.key === "q") hangupBtn.click();
  if (e.key === "s") spkBtn.click();
  if (e.key === "," && (e.metaKey || e.ctrlKey)) { e.preventDefault(); openSettings(); }
});

// Launch
autoConnect();
