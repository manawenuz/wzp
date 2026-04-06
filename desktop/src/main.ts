import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ── Elements ──
const connectScreen = document.getElementById("connect-screen")!;
const callScreen = document.getElementById("call-screen")!;
const roomInput = document.getElementById("room") as HTMLInputElement;
const aliasInput = document.getElementById("alias") as HTMLInputElement;
const osAecCheckbox = document.getElementById("os-aec") as HTMLInputElement;
const connectBtn = document.getElementById("connect-btn") as HTMLButtonElement;
const connectError = document.getElementById("connect-error")!;
const roomName = document.getElementById("room-name")!;
const callTimer = document.getElementById("call-timer")!;
const callStatus = document.getElementById("call-status")!;
const levelBar = document.getElementById("level-bar")!;
const participantsDiv = document.getElementById("participants")!;
const micBtn = document.getElementById("mic-btn")!;
const micIcon = document.getElementById("mic-icon")!;
const spkBtn = document.getElementById("spk-btn")!;
const spkIcon = document.getElementById("spk-icon")!;
const hangupBtn = document.getElementById("hangup-btn")!;
const statsDiv = document.getElementById("stats")!;
const myFingerprintEl = document.getElementById("my-fingerprint")!;
const recentRoomsDiv = document.getElementById("recent-rooms")!;

// Relay button
const relaySelected = document.getElementById("relay-selected")!;
const relayDot = document.getElementById("relay-dot")!;
const relayLabel = document.getElementById("relay-label")!;

// Relay dialog
const relayDialog = document.getElementById("relay-dialog")!;
const relayDialogClose = document.getElementById("relay-dialog-close")!;
const relayDialogList = document.getElementById("relay-dialog-list")!;
const relayAddName = document.getElementById("relay-add-name") as HTMLInputElement;
const relayAddAddr = document.getElementById("relay-add-addr") as HTMLInputElement;
const relayAddBtn = document.getElementById("relay-add-btn")!;

// Settings
const settingsPanel = document.getElementById("settings-panel")!;
const settingsClose = document.getElementById("settings-close")!;
const settingsSave = document.getElementById("settings-save")!;
const settingsBtnHome = document.getElementById("settings-btn-home")!;
const settingsBtnCall = document.getElementById("settings-btn-call")!;
const sRoom = document.getElementById("s-room") as HTMLInputElement;
const sAlias = document.getElementById("s-alias") as HTMLInputElement;
const sOsAec = document.getElementById("s-os-aec") as HTMLInputElement;
const sAgc = document.getElementById("s-agc") as HTMLInputElement;
const sFingerprint = document.getElementById("s-fingerprint")!;
const sRecentRooms = document.getElementById("s-recent-rooms")!;
const sClearRecent = document.getElementById("s-clear-recent")!;

let statusInterval: number | null = null;
let myFingerprint = "";
let userDisconnected = false;

// ── Data types ──
interface RelayServer {
  name: string;
  address: string;
  rtt?: number | null; // null = unknown, -1 = offline
}

interface RecentRoom {
  relay: string;
  room: string;
}

interface Settings {
  relays: RelayServer[];
  selectedRelay: number; // index into relays
  room: string;
  alias: string;
  osAec: boolean;
  agc: boolean;
  recentRooms: RecentRoom[];
}

function loadSettings(): Settings {
  const defaults: Settings = {
    relays: [{ name: "Default", address: "193.180.213.68:4433" }],
    selectedRelay: 0,
    room: "android",
    alias: "",
    osAec: true,
    agc: true,
    recentRooms: [],
  };
  try {
    const raw = localStorage.getItem("wzp-settings");
    if (raw) {
      const parsed = JSON.parse(raw);
      // Migrate: old format had relay as string
      if (parsed.relay && !parsed.relays) {
        parsed.relays = [{ name: "Default", address: parsed.relay }];
        parsed.selectedRelay = 0;
        delete parsed.relay;
      }
      // Migrate: old recentRooms as string[]
      if (parsed.recentRooms?.length > 0 && typeof parsed.recentRooms[0] === "string") {
        const addr = parsed.relays?.[0]?.address || defaults.relays[0].address;
        parsed.recentRooms = parsed.recentRooms.map((r: string) => ({ relay: addr, room: r }));
      }
      return { ...defaults, ...parsed };
    }
  } catch {}
  return defaults;
}

function saveSettingsObj(s: Settings) {
  localStorage.setItem("wzp-settings", JSON.stringify(s));
}

function getSelectedRelay(): RelayServer | undefined {
  const s = loadSettings();
  return s.relays[s.selectedRelay];
}

// ── Apply settings to form ──
function applySettings() {
  const s = loadSettings();
  roomInput.value = s.room;
  aliasInput.value = s.alias;
  osAecCheckbox.checked = s.osAec;
  renderRecentRooms(s.recentRooms);
  renderRelayButton();
}

// ── Relay dropdown ──
function dotClass(rtt: number | null | undefined): string {
  if (rtt === undefined || rtt === null) return "gray";
  if (rtt < 0) return "red";
  if (rtt > 200) return "yellow";
  return "green";
}

function rttText(rtt: number | null | undefined): string {
  if (rtt === undefined || rtt === null) return "";
  if (rtt < 0) return "offline";
  return `${rtt}ms`;
}

function renderRelayButton() {
  const s = loadSettings();
  const sel = s.relays[s.selectedRelay];
  if (sel) {
    relayDot.className = `dot ${dotClass(sel.rtt)}`;
    relayLabel.textContent = `${sel.name} (${sel.address})`;
  } else {
    relayDot.className = "dot gray";
    relayLabel.textContent = "No relay configured";
  }
}

relaySelected.addEventListener("click", () => openRelayDialog());

// ── Relay manage dialog ──
function openRelayDialog() {
  renderRelayDialogList();
  relayAddName.value = "";
  relayAddAddr.value = "";
  relayDialog.classList.remove("hidden");
}

function closeRelayDialog() {
  relayDialog.classList.add("hidden");
  renderRelayButton();
}

function renderRelayDialogList() {
  const s = loadSettings();
  relayDialogList.innerHTML = s.relays
    .map((r, i) => `
      <div class="relay-dialog-item ${i === s.selectedRelay ? "selected" : ""}" data-idx="${i}">
        <span class="dot ${dotClass(r.rtt)}"></span>
        <div class="relay-info">
          <div class="relay-name">${escapeHtml(r.name)}</div>
          <div class="relay-addr">${escapeHtml(r.address)}</div>
        </div>
        <span class="relay-rtt">${rttText(r.rtt)}</span>
        <button class="remove" data-idx="${i}">&times;</button>
      </div>`)
    .join("");

  // Click item to select
  relayDialogList.querySelectorAll(".relay-dialog-item").forEach((el) => {
    el.addEventListener("click", () => {
      const idx = parseInt((el as HTMLElement).dataset.idx || "0");
      const s = loadSettings();
      s.selectedRelay = idx;
      saveSettingsObj(s);
      renderRelayDialogList();
      renderRelayButton();
    });
  });

  // Click × to delete
  relayDialogList.querySelectorAll(".remove").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const idx = parseInt((btn as HTMLElement).dataset.idx || "0");
      const s = loadSettings();
      s.relays.splice(idx, 1);
      if (s.selectedRelay >= s.relays.length) s.selectedRelay = Math.max(0, s.relays.length - 1);
      saveSettingsObj(s);
      renderRelayDialogList();
      renderRelayButton();
    });
  });
}

relayAddBtn.addEventListener("click", () => {
  const name = relayAddName.value.trim();
  const addr = relayAddAddr.value.trim();
  if (!addr) return;
  const s = loadSettings();
  s.relays.push({ name: name || addr, address: addr });
  saveSettingsObj(s);
  relayAddName.value = "";
  relayAddAddr.value = "";
  renderRelayDialogList();
  pingAllRelays();
});

relayDialogClose.addEventListener("click", closeRelayDialog);
relayDialog.addEventListener("click", (e) => { if (e.target === relayDialog) closeRelayDialog(); });

// ── Ping all relays ──
async function pingAllRelays() {
  const s = loadSettings();
  for (let i = 0; i < s.relays.length; i++) {
    const r = s.relays[i];
    try {
      const rtt: number = await invoke("ping_relay", { relay: r.address });
      r.rtt = rtt;
    } catch {
      r.rtt = -1;
    }
  }
  saveSettingsObj(s);
  renderRelayButton();
  // Also update dialog if open
  if (!relayDialog.classList.contains("hidden")) {
    renderRelayDialogList();
  }
}

// ── Recent rooms ──
function renderRecentRooms(rooms: RecentRoom[]) {
  recentRoomsDiv.innerHTML = rooms
    .map((r) => `<span class="recent-room" data-relay="${escapeHtml(r.relay)}" data-room="${escapeHtml(r.room)}">${escapeHtml(r.room)}</span>`)
    .join("");
  recentRoomsDiv.querySelectorAll(".recent-room").forEach((el) => {
    el.addEventListener("click", () => {
      const ds = (el as HTMLElement).dataset;
      roomInput.value = ds.room || "";
      // Select matching relay
      const s = loadSettings();
      const idx = s.relays.findIndex((r) => r.address === ds.relay);
      if (idx >= 0) {
        s.selectedRelay = idx;
        saveSettingsObj(s);
        renderRelayButton();
      }
    });
  });
}

// ── Init ──
applySettings();
setTimeout(pingAllRelays, 300);

// Load fingerprint at startup
(async () => {
  try {
    const fp: string = await invoke("get_identity");
    myFingerprint = fp;
    myFingerprintEl.textContent = `ID: ${fp}`;
  } catch {}
})();

// Click fingerprint to copy
function copyFingerprint(el: HTMLElement) {
  if (myFingerprint) {
    navigator.clipboard.writeText(myFingerprint).then(() => {
      const orig = el.textContent;
      el.textContent = "Copied!";
      setTimeout(() => { el.textContent = orig; }, 1000);
    });
  }
}
myFingerprintEl.addEventListener("click", () => copyFingerprint(myFingerprintEl));
myFingerprintEl.style.cursor = "pointer";
sFingerprint.addEventListener("click", () => copyFingerprint(sFingerprint));
sFingerprint.style.cursor = "pointer";

// ── Connect ──
connectBtn.addEventListener("click", doConnect);
[roomInput, aliasInput].forEach((el) =>
  el.addEventListener("keydown", (e) => { if (e.key === "Enter") doConnect(); })
);

async function doConnect() {
  const relay = getSelectedRelay();
  if (!relay) {
    connectError.textContent = "No relay selected";
    return;
  }
  if (relay.rtt !== undefined && relay.rtt !== null && relay.rtt < 0) {
    connectError.textContent = "Relay is offline";
    return;
  }
  connectError.textContent = "";
  connectBtn.disabled = true;
  connectBtn.textContent = "Connecting...";
  userDisconnected = false;

  // Save recent room
  const s = loadSettings();
  s.room = roomInput.value;
  s.alias = aliasInput.value;
  s.osAec = osAecCheckbox.checked;
  const room = roomInput.value.trim();
  if (room) {
    const entry: RecentRoom = { relay: relay.address, room };
    s.recentRooms = [entry, ...s.recentRooms.filter((r) => !(r.relay === relay.address && r.room === room))].slice(0, 5);
  }
  saveSettingsObj(s);

  try {
    await invoke("connect", {
      relay: relay.address,
      room: roomInput.value,
      alias: aliasInput.value,
      osAec: osAecCheckbox.checked,
    });
    showCallScreen();
  } catch (e: any) {
    connectError.textContent = String(e);
    connectBtn.disabled = false;
    connectBtn.textContent = "Connect";
  }
}

function showCallScreen() {
  connectScreen.classList.add("hidden");
  callScreen.classList.remove("hidden");
  roomName.textContent = roomInput.value;
  callStatus.className = "status-dot";
  statusInterval = window.setInterval(pollStatus, 250);
}

function showConnectScreen() {
  callScreen.classList.add("hidden");
  connectScreen.classList.remove("hidden");
  connectBtn.disabled = false;
  connectBtn.textContent = "Connect";
  levelBar.style.width = "0%";
  if (statusInterval) { clearInterval(statusInterval); statusInterval = null; }
}

// ── Mute / hangup ──
micBtn.addEventListener("click", async () => {
  try {
    const muted: boolean = await invoke("toggle_mic");
    micBtn.classList.toggle("muted", muted);
    micIcon.textContent = muted ? "Mic Off" : "Mic";
  } catch {}
});

spkBtn.addEventListener("click", async () => {
  try {
    const muted: boolean = await invoke("toggle_speaker");
    spkBtn.classList.toggle("muted", muted);
    spkIcon.textContent = muted ? "Spk Off" : "Spk";
  } catch {}
});

hangupBtn.addEventListener("click", async () => {
  userDisconnected = true;
  try { await invoke("disconnect"); } catch {}
  showConnectScreen();
});

document.addEventListener("keydown", (e) => {
  if (callScreen.classList.contains("hidden")) return;
  if ((e.target as HTMLElement).tagName === "INPUT") return;
  if (e.key === "m") micBtn.click();
  if (e.key === "s") spkBtn.click();
  if (e.key === "q") hangupBtn.click();
});

// ── Status polling ──
interface CallStatusI {
  active: boolean;
  mic_muted: boolean;
  spk_muted: boolean;
  participants: { fingerprint: string; alias: string | null }[];
  encode_fps: number;
  recv_fps: number;
  audio_level: number;
  call_duration_secs: number;
  fingerprint: string;
}

function formatDuration(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

let reconnectAttempts = 0;
const MAX_RECONNECT = 5;

async function pollStatus() {
  try {
    const st: CallStatusI = await invoke("get_status");
    if (!st.active) {
      if (!userDisconnected && reconnectAttempts < MAX_RECONNECT) {
        reconnectAttempts++;
        const delay = Math.min(1000 * Math.pow(2, reconnectAttempts - 1), 10000);
        callStatus.className = "status-dot reconnecting";
        statsDiv.textContent = `Reconnecting (${reconnectAttempts}/${MAX_RECONNECT})...`;
        const relay = getSelectedRelay();
        if (relay) {
          setTimeout(async () => {
            try {
              await invoke("connect", {
                relay: relay.address, room: roomInput.value,
                alias: aliasInput.value, osAec: osAecCheckbox.checked,
              });
              reconnectAttempts = 0;
              callStatus.className = "status-dot";
            } catch {}
          }, delay);
        }
        return;
      }
      reconnectAttempts = 0;
      showConnectScreen();
      return;
    }

    reconnectAttempts = 0;
    if (st.fingerprint) myFingerprint = st.fingerprint;

    micBtn.classList.toggle("muted", st.mic_muted);
    micIcon.textContent = st.mic_muted ? "Mic Off" : "Mic";
    spkBtn.classList.toggle("muted", st.spk_muted);
    spkIcon.textContent = st.spk_muted ? "Spk Off" : "Spk";

    callTimer.textContent = formatDuration(st.call_duration_secs);

    const rms = st.audio_level;
    const pct = rms > 0 ? Math.min(100, (Math.log(rms) / Math.log(32767)) * 100) : 0;
    levelBar.style.width = `${pct}%`;

    if (st.participants.length === 0) {
      participantsDiv.innerHTML = '<div class="participants-empty">Waiting for participants...</div>';
    } else {
      participantsDiv.innerHTML = st.participants.map((p) => {
        const name = p.alias || "Anonymous";
        const initial = name.charAt(0).toUpperCase();
        const fp = p.fingerprint ? p.fingerprint.substring(0, 16) : "";
        const isMe = p.fingerprint && myFingerprint.includes(p.fingerprint);
        return `
          <div class="participant">
            <div class="avatar ${isMe ? "me" : ""}">${initial}</div>
            <div class="info">
              <div class="name">${escapeHtml(name)} ${isMe ? '<span class="you-badge">you</span>' : ""}</div>
              <div class="fp">${escapeHtml(fp)}</div>
            </div>
          </div>`;
      }).join("");
    }

    statsDiv.textContent = `TX: ${st.encode_fps} | RX: ${st.recv_fps}`;
  } catch {}
}

function escapeHtml(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

listen("call-event", (event: any) => {
  const { kind } = event.payload;
  if (kind === "room-update") pollStatus();
  if (kind === "disconnected" && !userDisconnected) pollStatus();
});

// ── Settings panel ──
function openSettings() {
  const s = loadSettings();
  sRoom.value = s.room;
  sAlias.value = s.alias;
  sOsAec.checked = s.osAec;
  sFingerprint.textContent = myFingerprint || "(loading...)";
  renderSettingsRecentRooms(s.recentRooms);
  settingsPanel.classList.remove("hidden");
}

function closeSettings() { settingsPanel.classList.add("hidden"); }

function renderSettingsRecentRooms(rooms: RecentRoom[]) {
  if (rooms.length === 0) {
    sRecentRooms.innerHTML = '<span style="color:var(--text-dim);font-size:12px">No recent rooms</span>';
    return;
  }
  sRecentRooms.innerHTML = rooms.map((r, i) => `
    <div class="recent-room-item">
      <span>${escapeHtml(r.room)} <small style="color:var(--text-dim)">${escapeHtml(r.relay)}</small></span>
      <button class="remove" data-idx="${i}">&times;</button>
    </div>`).join("");
  sRecentRooms.querySelectorAll(".remove").forEach((btn) => {
    btn.addEventListener("click", () => {
      const idx = parseInt((btn as HTMLElement).dataset.idx || "0");
      const s = loadSettings();
      s.recentRooms.splice(idx, 1);
      saveSettingsObj(s);
      renderSettingsRecentRooms(s.recentRooms);
    });
  });
}

settingsBtnHome.addEventListener("click", openSettings);
settingsBtnCall.addEventListener("click", openSettings);
settingsClose.addEventListener("click", closeSettings);
settingsPanel.addEventListener("click", (e) => { if (e.target === settingsPanel) closeSettings(); });

settingsSave.addEventListener("click", () => {
  const s = loadSettings();
  s.room = sRoom.value;
  s.alias = sAlias.value;
  s.osAec = sOsAec.checked;
  saveSettingsObj(s);
  roomInput.value = s.room;
  aliasInput.value = s.alias;
  osAecCheckbox.checked = s.osAec;
  renderRecentRooms(s.recentRooms);
  closeSettings();
});

sClearRecent.addEventListener("click", () => {
  const s = loadSettings();
  s.recentRooms = [];
  saveSettingsObj(s);
  renderSettingsRecentRooms([]);
  renderRecentRooms([]);
});

document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === ",") {
    e.preventDefault();
    settingsPanel.classList.contains("hidden") ? openSettings() : closeSettings();
  }
  if (e.key === "Escape") {
    if (!relayDialog.classList.contains("hidden")) closeRelayDialog();
    else if (!settingsPanel.classList.contains("hidden")) closeSettings();
  }
});
