import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// Elements
const connectScreen = document.getElementById("connect-screen")!;
const callScreen = document.getElementById("call-screen")!;
const relayInput = document.getElementById("relay") as HTMLInputElement;
const roomInput = document.getElementById("room") as HTMLInputElement;
const aliasInput = document.getElementById("alias") as HTMLInputElement;
const osAecCheckbox = document.getElementById("os-aec") as HTMLInputElement;
const connectBtn = document.getElementById("connect-btn") as HTMLButtonElement;
const connectError = document.getElementById("connect-error")!;
const roomName = document.getElementById("room-name")!;
const participantsDiv = document.getElementById("participants")!;
const micBtn = document.getElementById("mic-btn")!;
const spkBtn = document.getElementById("spk-btn")!;
const hangupBtn = document.getElementById("hangup-btn")!;
const statsDiv = document.getElementById("stats")!;

let statusInterval: number | null = null;

// Load saved settings
const saved = localStorage.getItem("wzp-settings");
if (saved) {
  try {
    const s = JSON.parse(saved);
    if (s.relay) relayInput.value = s.relay;
    if (s.room) roomInput.value = s.room;
    if (s.alias) aliasInput.value = s.alias;
    if (s.osAec !== undefined) osAecCheckbox.checked = s.osAec;
  } catch {}
}

function saveSettings() {
  localStorage.setItem(
    "wzp-settings",
    JSON.stringify({
      relay: relayInput.value,
      room: roomInput.value,
      alias: aliasInput.value,
      osAec: osAecCheckbox.checked,
    })
  );
}

// Connect
connectBtn.addEventListener("click", async () => {
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
});

function showCallScreen() {
  connectScreen.classList.add("hidden");
  callScreen.classList.remove("hidden");
  roomName.textContent = roomInput.value;

  // Poll status
  statusInterval = window.setInterval(pollStatus, 500);
}

function showConnectScreen() {
  callScreen.classList.add("hidden");
  connectScreen.classList.remove("hidden");
  connectBtn.disabled = false;
  connectBtn.textContent = "Connect";
  if (statusInterval) {
    clearInterval(statusInterval);
    statusInterval = null;
  }
}

// Mute buttons
micBtn.addEventListener("click", async () => {
  try {
    const muted: boolean = await invoke("toggle_mic");
    micBtn.classList.toggle("muted", muted);
  } catch {}
});

spkBtn.addEventListener("click", async () => {
  try {
    const muted: boolean = await invoke("toggle_speaker");
    spkBtn.classList.toggle("muted", muted);
  } catch {}
});

// Hangup
hangupBtn.addEventListener("click", async () => {
  try {
    await invoke("disconnect");
  } catch {}
  showConnectScreen();
});

// Keyboard shortcuts
document.addEventListener("keydown", (e) => {
  if (callScreen.classList.contains("hidden")) return;
  if (e.key === "m") micBtn.click();
  if (e.key === "s") spkBtn.click();
  if (e.key === "q") hangupBtn.click();
});

// Status polling
interface CallStatus {
  active: boolean;
  mic_muted: boolean;
  spk_muted: boolean;
  participants: { fingerprint: string; alias: string | null }[];
  encode_fps: number;
  recv_fps: number;
}

async function pollStatus() {
  try {
    const status: CallStatus = await invoke("get_status");
    if (!status.active) {
      showConnectScreen();
      return;
    }

    // Update mute state
    micBtn.classList.toggle("muted", status.mic_muted);
    spkBtn.classList.toggle("muted", status.spk_muted);

    // Update participants
    participantsDiv.innerHTML = status.participants
      .map((p) => {
        const name = p.alias || "Anonymous";
        const initial = name.charAt(0).toUpperCase();
        const fp = p.fingerprint
          ? p.fingerprint.substring(0, 8) + "..."
          : "";
        return `
          <div class="participant">
            <div class="avatar">${initial}</div>
            <div class="info">
              <div class="name">${escapeHtml(name)}</div>
              <div class="fp">${escapeHtml(fp)}</div>
            </div>
          </div>`;
      })
      .join("");

    // Stats
    statsDiv.textContent = `TX: ${status.encode_fps} frames | RX: ${status.recv_fps} frames`;
  } catch {}
}

function escapeHtml(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Listen for events from backend
listen("call-event", (event: any) => {
  const { kind, message } = event.payload;
  if (kind === "room-update") {
    pollStatus();
  }
});
