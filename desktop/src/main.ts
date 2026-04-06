import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ── Elements ──
const connectScreen = document.getElementById("connect-screen")!;
const callScreen = document.getElementById("call-screen")!;
const relayInput = document.getElementById("relay") as HTMLInputElement;
const roomInput = document.getElementById("room") as HTMLInputElement;
const aliasInput = document.getElementById("alias") as HTMLInputElement;
const osAecCheckbox = document.getElementById("os-aec") as HTMLInputElement;
const connectBtn = document.getElementById("connect-btn") as HTMLButtonElement;
const connectError = document.getElementById("connect-error")!;
const roomName = document.getElementById("room-name")!;
const callTimer = document.getElementById("call-timer")!;
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

let statusInterval: number | null = null;
let myFingerprint = "";

// ── Settings persistence ──
interface Settings {
  relay: string;
  room: string;
  alias: string;
  osAec: boolean;
  recentRooms: string[];
}

function loadSettings(): Settings {
  const defaults: Settings = {
    relay: "193.180.213.68:4433",
    room: "android",
    alias: "",
    osAec: true,
    recentRooms: [],
  };
  try {
    const raw = localStorage.getItem("wzp-settings");
    if (raw) return { ...defaults, ...JSON.parse(raw) };
  } catch {}
  return defaults;
}

function saveSettings() {
  const s = loadSettings();
  s.relay = relayInput.value;
  s.room = roomInput.value;
  s.alias = aliasInput.value;
  s.osAec = osAecCheckbox.checked;
  // Add room to recent list (dedup, max 5)
  const room = roomInput.value.trim();
  if (room) {
    s.recentRooms = [room, ...s.recentRooms.filter((r) => r !== room)].slice(
      0,
      5
    );
  }
  localStorage.setItem("wzp-settings", JSON.stringify(s));
}

function applySettings() {
  const s = loadSettings();
  relayInput.value = s.relay;
  roomInput.value = s.room;
  aliasInput.value = s.alias;
  osAecCheckbox.checked = s.osAec;
  renderRecentRooms(s.recentRooms);
}

function renderRecentRooms(rooms: string[]) {
  recentRoomsDiv.innerHTML = rooms
    .map(
      (r) =>
        `<span class="recent-room" data-room="${escapeHtml(r)}">${escapeHtml(r)}</span>`
    )
    .join("");
  recentRoomsDiv.querySelectorAll(".recent-room").forEach((el) => {
    el.addEventListener("click", () => {
      roomInput.value = (el as HTMLElement).dataset.room || "";
    });
  });
}

applySettings();

// ── Connect ──
connectBtn.addEventListener("click", doConnect);
// Enter key to connect
[relayInput, roomInput, aliasInput].forEach((el) =>
  el.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doConnect();
  })
);

async function doConnect() {
  connectError.textContent = "";
  connectBtn.disabled = true;
  connectBtn.textContent = "Connecting...";
  saveSettings();

  try {
    await invoke("connect", {
      relay: relayInput.value,
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
  statusInterval = window.setInterval(pollStatus, 250);
}

function showConnectScreen() {
  callScreen.classList.add("hidden");
  connectScreen.classList.remove("hidden");
  connectBtn.disabled = false;
  connectBtn.textContent = "Connect";
  levelBar.style.width = "0%";
  if (statusInterval) {
    clearInterval(statusInterval);
    statusInterval = null;
  }
}

// ── Mute buttons ──
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
  try {
    await invoke("disconnect");
  } catch {}
  showConnectScreen();
});

// Keyboard shortcuts (only when in call, and not typing in an input)
document.addEventListener("keydown", (e) => {
  if (callScreen.classList.contains("hidden")) return;
  if ((e.target as HTMLElement).tagName === "INPUT") return;
  if (e.key === "m") micBtn.click();
  if (e.key === "s") spkBtn.click();
  if (e.key === "q") hangupBtn.click();
});

// ── Status polling ──
interface CallStatus {
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

async function pollStatus() {
  try {
    const st: CallStatus = await invoke("get_status");
    if (!st.active) {
      showConnectScreen();
      return;
    }

    myFingerprint = st.fingerprint;
    myFingerprintEl.textContent = st.fingerprint
      ? `ID: ${st.fingerprint}`
      : "";

    // Mute state
    micBtn.classList.toggle("muted", st.mic_muted);
    micIcon.textContent = st.mic_muted ? "Mic Off" : "Mic";
    spkBtn.classList.toggle("muted", st.spk_muted);
    spkIcon.textContent = st.spk_muted ? "Spk Off" : "Spk";

    // Timer
    callTimer.textContent = formatDuration(st.call_duration_secs);

    // Audio level (RMS 0–32767 → percentage, log scale)
    const rms = st.audio_level;
    const pct = rms > 0 ? Math.min(100, (Math.log(rms) / Math.log(32767)) * 100) : 0;
    levelBar.style.width = `${pct}%`;

    // Participants
    if (st.participants.length === 0) {
      participantsDiv.innerHTML =
        '<div class="participants-empty">Waiting for participants...</div>';
    } else {
      participantsDiv.innerHTML = st.participants
        .map((p) => {
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
        })
        .join("");
    }

    // Stats
    statsDiv.textContent = `TX: ${st.encode_fps} | RX: ${st.recv_fps}`;
  } catch {}
}

function escapeHtml(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// ── Events from backend ──
listen("call-event", (event: any) => {
  const { kind } = event.payload;
  if (kind === "room-update") pollStatus();
});
