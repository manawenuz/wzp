import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { generateIdenticon, createIdenticonEl } from "./identicon";

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
const myIdenticonEl = document.getElementById("my-identicon")!;
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
const sQuality = document.getElementById("s-quality") as HTMLInputElement;
const sQualityLabel = document.getElementById("s-quality-label")!;

// Quality slider config — best (left/green) to worst (right/red)
const QUALITY_STEPS = ["studio-64k", "studio-48k", "studio-32k", "auto", "good", "degraded", "codec2-3200", "catastrophic"];
const QUALITY_LABELS = ["Studio 64k", "Studio 48k", "Studio 32k", "Auto", "Opus 24k", "Opus 6k", "Codec2 3.2k", "Codec2 1.2k"];
const QUALITY_COLORS = ["#22c55e", "#4ade80", "#86efac", "#a3e635", "#facc15", "#f59e0b", "#e97320", "#991b1b"];

function qualityToIndex(q: string): number {
  const idx = QUALITY_STEPS.indexOf(q);
  return idx >= 0 ? idx : 3; // default to "auto" (index 3)
}

function updateQualityUI(index: number) {
  sQualityLabel.textContent = QUALITY_LABELS[index];
  sQualityLabel.style.color = QUALITY_COLORS[index];
  sQuality.style.background = `linear-gradient(90deg, #22c55e 0%, #86efac 25%, #facc15 50%, #e97320 75%, #991b1b 100%)`;
}

sQuality.addEventListener("input", () => {
  updateQualityUI(parseInt(sQuality.value));
});
const sFingerprint = document.getElementById("s-fingerprint")!;
const sRecentRooms = document.getElementById("s-recent-rooms")!;
const sClearRecent = document.getElementById("s-clear-recent")!;

// Key warning dialog
const keyWarning = document.getElementById("key-warning")!;
const kwOldFp = document.getElementById("kw-old-fp")!;
const kwNewFp = document.getElementById("kw-new-fp")!;
const kwAccept = document.getElementById("kw-accept")!;
const kwCancel = document.getElementById("kw-cancel")!;

let statusInterval: number | null = null;
let myFingerprint = "";
let userDisconnected = false;

// ── Data types ──
interface RelayServer {
  name: string;
  address: string;
  rtt?: number | null;
  serverFingerprint?: string | null;    // from ping
  knownFingerprint?: string | null;     // saved TOFU fingerprint
}

interface RecentRoom { relay: string; room: string; }

interface Settings {
  relays: RelayServer[];
  selectedRelay: number;
  room: string;
  alias: string;
  osAec: boolean;
  agc: boolean;
  quality: string;
  recentRooms: RecentRoom[];
}

function loadSettings(): Settings {
  const defaults: Settings = {
    relays: [
      // Local laptop relay — used during Android rewrite testing so the phone
      // and the relay logs are on the same host. Laptop IP on the test LAN.
      { name: "Laptop", address: "172.16.81.125:4433" },
      { name: "Default", address: "193.180.213.68:4433" },
    ],
    selectedRelay: 0, room: "android", alias: "",
    osAec: true, agc: true, quality: "auto", recentRooms: [],
  };
  try {
    const raw = localStorage.getItem("wzp-settings");
    if (raw) {
      const parsed = JSON.parse(raw);
      if (parsed.relay && !parsed.relays) {
        parsed.relays = [{ name: "Default", address: parsed.relay }];
        parsed.selectedRelay = 0;
        delete parsed.relay;
      }
      if (parsed.recentRooms?.length > 0 && typeof parsed.recentRooms[0] === "string") {
        const addr = parsed.relays?.[0]?.address || defaults.relays[0].address;
        parsed.recentRooms = parsed.recentRooms.map((r: string) => ({ relay: addr, room: r }));
      }
      // Ensure the Laptop test relay is present as the first entry for
      // existing installs — otherwise users with cached settings keep using
      // the remote default and we have to manually add it each install.
      // Remove this block once the Android rewrite is stable.
      const LAPTOP_ADDR = "172.16.81.125:4433";
      if (Array.isArray(parsed.relays) && !parsed.relays.some((r: any) => r.address === LAPTOP_ADDR)) {
        parsed.relays.unshift({ name: "Laptop", address: LAPTOP_ADDR });
        parsed.selectedRelay = 0;
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

// ── Helpers ──
function escapeHtml(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// ── Lock status ──
type LockStatus = "verified" | "new" | "changed" | "offline" | "unknown";

function lockStatus(relay: RelayServer): LockStatus {
  if (relay.rtt === undefined || relay.rtt === null) return "unknown";
  if (relay.rtt < 0) return "offline";
  if (!relay.serverFingerprint) return "new";
  if (!relay.knownFingerprint) return "new"; // first time
  if (relay.serverFingerprint === relay.knownFingerprint) return "verified";
  return "changed";
}

function lockIcon(status: LockStatus): string {
  switch (status) {
    case "verified": return "🔒";
    case "new": return "🔓";
    case "changed": return "⚠️";
    case "offline": return "🔴";
    case "unknown": return "⚪";
  }
}

function lockColor(status: LockStatus): string {
  switch (status) {
    case "verified": return "var(--green)";
    case "new": return "var(--yellow)";
    case "changed": return "var(--red)";
    case "offline": return "var(--red)";
    case "unknown": return "var(--text-dim)";
  }
}

// ── Apply settings ──
function applySettings() {
  const s = loadSettings();
  roomInput.value = s.room;
  aliasInput.value = s.alias;
  osAecCheckbox.checked = s.osAec;
  renderRecentRooms(s.recentRooms);
  renderRelayButton();
}

// ── Relay button ──
function renderRelayButton() {
  const s = loadSettings();
  const sel = s.relays[s.selectedRelay];
  if (sel) {
    const ls = lockStatus(sel);
    relayDot.textContent = lockIcon(ls);
    relayDot.className = "relay-lock";
    relayLabel.textContent = `${sel.name} (${sel.address})`;
  } else {
    relayDot.textContent = "⚪";
    relayDot.className = "relay-lock";
    relayLabel.textContent = "No relay configured";
  }
}

relaySelected.addEventListener("click", () => openRelayDialog());

// ── Relay dialog ──
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
  relayDialogList.innerHTML = "";
  s.relays.forEach((r, i) => {
    const item = document.createElement("div");
    item.className = `relay-dialog-item ${i === s.selectedRelay ? "selected" : ""}`;

    const ls = lockStatus(r);
    const fp = r.serverFingerprint || r.address;

    // Identicon
    const icon = createIdenticonEl(fp, 32, true);
    icon.title = r.serverFingerprint
      ? `Server: ${r.serverFingerprint}\nClick to copy`
      : `No fingerprint yet`;
    item.appendChild(icon);

    // Info
    const info = document.createElement("div");
    info.className = "relay-info";
    info.innerHTML = `
      <div class="relay-name">${escapeHtml(r.name)}</div>
      <div class="relay-addr">${escapeHtml(r.address)}</div>
    `;
    item.appendChild(info);

    // Lock + RTT
    const meta = document.createElement("div");
    meta.className = "relay-meta";
    const rttStr = r.rtt !== undefined && r.rtt !== null
      ? (r.rtt < 0 ? "offline" : `${r.rtt}ms`)
      : "";
    meta.innerHTML = `
      <span class="relay-lock-icon" style="color:${lockColor(ls)}">${lockIcon(ls)}</span>
      <span class="relay-rtt">${rttStr}</span>
    `;
    item.appendChild(meta);

    // Delete button
    const del = document.createElement("button");
    del.className = "remove";
    del.textContent = "×";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      const s = loadSettings();
      s.relays.splice(i, 1);
      if (s.selectedRelay >= s.relays.length) s.selectedRelay = Math.max(0, s.relays.length - 1);
      saveSettingsObj(s);
      renderRelayDialogList();
      renderRelayButton();
    });
    item.appendChild(del);

    // Click to select
    item.addEventListener("click", () => {
      const s = loadSettings();
      s.selectedRelay = i;

      // TOFU: if first time seeing this server, trust its fingerprint
      if (r.serverFingerprint && !r.knownFingerprint) {
        s.relays[i].knownFingerprint = r.serverFingerprint;
      }

      saveSettingsObj(s);
      renderRelayDialogList();
      renderRelayButton();
    });

    relayDialogList.appendChild(item);
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

// ── Ping ──
interface PingResult { rtt_ms: number; server_fingerprint: string; }

async function pingAllRelays() {
  const s = loadSettings();
  for (let i = 0; i < s.relays.length; i++) {
    const r = s.relays[i];
    try {
      const result: PingResult = await invoke("ping_relay", { relay: r.address });
      r.rtt = result.rtt_ms;
      r.serverFingerprint = result.server_fingerprint;

      // TOFU: auto-save fingerprint on first contact
      if (!r.knownFingerprint) {
        r.knownFingerprint = result.server_fingerprint;
      }
    } catch {
      r.rtt = -1;
    }
  }
  saveSettingsObj(s);
  renderRelayButton();
  if (!relayDialog.classList.contains("hidden")) renderRelayDialogList();
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
      const s = loadSettings();
      const idx = s.relays.findIndex((r) => r.address === ds.relay);
      if (idx >= 0) { s.selectedRelay = idx; saveSettingsObj(s); renderRelayButton(); }
    });
  });
}

// ── Init ──
applySettings();
setTimeout(pingAllRelays, 300);

// Load fingerprint + alias + git hash + render identicon
interface AppInfo { git_hash: string; alias: string; fingerprint: string; data_dir: string }

(async () => {
  try {
    const info: AppInfo = await invoke("get_app_info");
    const fp = info.fingerprint;
    myFingerprint = fp;
    myFingerprintEl.textContent = fp;
    myFingerprintEl.style.cursor = "pointer";
    myFingerprintEl.addEventListener("click", () => {
      navigator.clipboard.writeText(fp).then(() => {
        const orig = myFingerprintEl.textContent;
        myFingerprintEl.textContent = "Copied!";
        setTimeout(() => { myFingerprintEl.textContent = orig; }, 1000);
      });
    });

    // Identicon next to fingerprint
    const icon = createIdenticonEl(fp, 28, true);
    myIdenticonEl.innerHTML = "";
    myIdenticonEl.appendChild(icon);

    // Prefill alias if the user hasn't typed one yet
    if (!aliasInput.value.trim()) {
      aliasInput.value = info.alias;
      const s = loadSettings();
      s.alias = info.alias;
      saveSettingsObj(s);
    }

    // Stamp the build hash on the home screen so we can prove which build
    // is installed (this caused us a lot of grief on the Kotlin app).
    let buildEl = document.getElementById("build-hash");
    if (!buildEl) {
      buildEl = document.createElement("div");
      buildEl.id = "build-hash";
      buildEl.style.cssText = "font-size:10px;opacity:0.6;text-align:center;margin-top:4px;font-family:monospace";
      myFingerprintEl.parentElement?.appendChild(buildEl);
    }
    buildEl.textContent = `build ${info.git_hash} • ${info.alias}`;
    buildEl.title = info.data_dir;
  } catch (e) {
    console.error("get_app_info failed", e);
  }
})();

// ── Connect ──
connectBtn.addEventListener("click", doConnect);
[roomInput, aliasInput].forEach((el) =>
  el.addEventListener("keydown", (e) => { if (e.key === "Enter") doConnect(); })
);

function showKeyWarning(oldFp: string, newFp: string): Promise<boolean> {
  return new Promise((resolve) => {
    kwOldFp.textContent = oldFp;
    kwNewFp.textContent = newFp;
    keyWarning.classList.remove("hidden");

    const cleanup = () => {
      keyWarning.classList.add("hidden");
      kwAccept.removeEventListener("click", onAccept);
      kwCancel.removeEventListener("click", onCancel);
      keyWarning.removeEventListener("click", onBackdrop);
    };
    const onAccept = () => { cleanup(); resolve(true); };
    const onCancel = () => { cleanup(); resolve(false); };
    const onBackdrop = (e: Event) => { if (e.target === keyWarning) { cleanup(); resolve(false); } };

    kwAccept.addEventListener("click", onAccept);
    kwCancel.addEventListener("click", onCancel);
    keyWarning.addEventListener("click", onBackdrop);
  });
}

async function doConnect() {
  const relay = getSelectedRelay();
  if (!relay) { connectError.textContent = "No relay selected"; return; }

  // Warn on fingerprint mismatch
  const ls = lockStatus(relay);
  if (ls === "changed") {
    const accepted = await showKeyWarning(relay.knownFingerprint || "", relay.serverFingerprint || "");
    if (!accepted) return;
    // User accepted — update known fingerprint
    const s = loadSettings();
    s.relays[s.selectedRelay].knownFingerprint = relay.serverFingerprint;
    saveSettingsObj(s);
    renderRelayButton();
  }

  // Don't block connect on offline — ping may have failed transiently

  connectError.textContent = "";
  connectBtn.disabled = true;
  connectBtn.textContent = "Connecting...";
  userDisconnected = false;

  const s = loadSettings();
  s.room = roomInput.value; s.alias = aliasInput.value; s.osAec = osAecCheckbox.checked;
  const room = roomInput.value.trim();
  if (room) {
    const entry: RecentRoom = { relay: relay.address, room };
    s.recentRooms = [entry, ...s.recentRooms.filter((r) => !(r.relay === relay.address && r.room === room))].slice(0, 5);
  }
  saveSettingsObj(s);

  try {
    await invoke("connect", {
      relay: relay.address, room: roomInput.value,
      alias: aliasInput.value, osAec: osAecCheckbox.checked,
      quality: s.quality || "auto",
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
  try { const m: boolean = await invoke("toggle_mic"); micBtn.classList.toggle("muted", m); micIcon.textContent = m ? "Mic Off" : "Mic"; } catch {}
});

// Speaker routing (Android) — toggles AudioManager.setSpeakerphoneOn so the
// same Oboe VoiceCommunication stream swaps between earpiece and
// loudspeaker without restarting. Desktop callers get a no-op command so
// the same UI works everywhere.
let speakerphoneOn = false;
function updateSpkLabel() {
  spkBtn.classList.toggle("muted", !speakerphoneOn);
  spkIcon.textContent = speakerphoneOn ? "Speaker" : "Earpiece";
}
spkBtn.addEventListener("click", async () => {
  const next = !speakerphoneOn;
  try {
    await invoke("set_speakerphone", { on: next });
    speakerphoneOn = next;
    updateSpkLabel();
  } catch (e) {
    console.error("set_speakerphone failed:", e);
  }
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
  active: boolean; mic_muted: boolean; spk_muted: boolean;
  participants: { fingerprint: string; alias: string | null }[];
  encode_fps: number; recv_fps: number; audio_level: number;
  call_duration_secs: number; fingerprint: string;
}

function formatDuration(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

let reconnectAttempts = 0;

async function pollStatus() {
  try {
    const st: CallStatusI = await invoke("get_status");
    if (!st.active) {
      if (!userDisconnected && reconnectAttempts < 5) {
        reconnectAttempts++;
        callStatus.className = "status-dot reconnecting";
        statsDiv.textContent = `Reconnecting (${reconnectAttempts}/5)...`;
        const relay = getSelectedRelay();
        if (relay) {
          const delay = Math.min(1000 * Math.pow(2, reconnectAttempts - 1), 10000);
          setTimeout(async () => {
            try {
              await invoke("connect", { relay: relay.address, room: roomInput.value, alias: aliasInput.value, osAec: osAecCheckbox.checked });
              reconnectAttempts = 0; callStatus.className = "status-dot";
            } catch {}
          }, delay);
        }
        return;
      }
      reconnectAttempts = 0; showConnectScreen(); return;
    }

    reconnectAttempts = 0;
    if (st.fingerprint) myFingerprint = st.fingerprint;

    micBtn.classList.toggle("muted", st.mic_muted);
    micIcon.textContent = st.mic_muted ? "Mic Off" : "Mic";
    // NB: spkBtn label is driven by the Android audio routing state
    // (speakerphoneOn / updateSpkLabel), not by the engine's spk_muted.
    // Skip that here so pollStatus doesn't clobber the routing UI.
    callTimer.textContent = formatDuration(st.call_duration_secs);

    const rms = st.audio_level;
    const pct = rms > 0 ? Math.min(100, (Math.log(rms) / Math.log(32767)) * 100) : 0;
    levelBar.style.width = `${pct}%`;

    // Participants grouped by relay
    if (st.participants.length === 0) {
      participantsDiv.innerHTML = '<div class="participants-empty">Waiting for participants...</div>';
    } else {
      participantsDiv.innerHTML = "";
      // Group by relay_label (null = this relay)
      const groups: Record<string, typeof st.participants> = {};
      st.participants.forEach((p: any) => {
        const relay = p.relay_label || "This Relay";
        if (!groups[relay]) groups[relay] = [];
        groups[relay].push(p);
      });

      Object.entries(groups).forEach(([relay, members]) => {
        // Relay header
        const header = document.createElement("div");
        header.className = "relay-group-header";
        const isLocal = relay === "This Relay";
        header.innerHTML = `<span class="relay-dot-small ${isLocal ? "green" : "blue"}"></span> ${escapeHtml(relay)}`;
        participantsDiv.appendChild(header);

        // Participants under this relay
        (members as any[]).forEach((p) => {
          const name = p.alias || "Anonymous";
          const fp = p.fingerprint || "";
          const isMe = fp && myFingerprint.includes(fp);

          const row = document.createElement("div");
          row.className = "participant";

          const icon = createIdenticonEl(fp || name, 36, true);
          if (isMe) icon.style.outline = "2px solid var(--accent)";
          row.appendChild(icon);

          const info = document.createElement("div");
          info.className = "info";
          info.innerHTML = `
            <div class="name">${escapeHtml(name)} ${isMe ? '<span class="you-badge">you</span>' : ""}</div>
            <div class="fp">${escapeHtml(fp ? fp.substring(0, 16) : "")}</div>
          `;
          row.appendChild(info);
          participantsDiv.appendChild(row);
        });
      });
    }

    // Stats line with codec badges
    const txBadge = (st as any).tx_codec ? `<span class="codec-badge tx">${escapeHtml((st as any).tx_codec)}</span>` : "";
    const rxBadge = (st as any).rx_codec ? `<span class="codec-badge rx">${escapeHtml((st as any).rx_codec)}</span>` : "";
    statsDiv.innerHTML = `${txBadge} ${rxBadge} TX: ${st.encode_fps} | RX: ${st.recv_fps}`;
  } catch {}
}

listen("call-event", (event: any) => {
  const { kind } = event.payload;
  if (kind === "room-update") pollStatus();
  if (kind === "disconnected" && !userDisconnected) pollStatus();
});

// ── Settings ──
function openSettings() {
  const s = loadSettings();
  sRoom.value = s.room; sAlias.value = s.alias; sOsAec.checked = s.osAec;
  const qi = qualityToIndex(s.quality || "auto");
  sQuality.value = String(qi);
  updateQualityUI(qi);
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
      <button class="remove" data-idx="${i}">×</button>
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
  s.room = sRoom.value; s.alias = sAlias.value; s.osAec = sOsAec.checked;
  s.quality = QUALITY_STEPS[parseInt(sQuality.value)] || "auto";
  saveSettingsObj(s);
  roomInput.value = s.room; aliasInput.value = s.alias; osAecCheckbox.checked = s.osAec;
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

// ── Direct Calling UI ──
const modeRoom = document.getElementById("mode-room")!;
const modeDirect = document.getElementById("mode-direct")!;
const roomModeDiv = document.getElementById("room-mode")!;
const directModeDiv = document.getElementById("direct-mode")!;
const registerBtn = document.getElementById("register-btn") as HTMLButtonElement;
const directRegistered = document.getElementById("direct-registered")!;
const incomingCallPanel = document.getElementById("incoming-call-panel")!;
const incomingCaller = document.getElementById("incoming-caller")!;
const acceptCallBtn = document.getElementById("accept-call-btn")!;
const rejectCallBtn = document.getElementById("reject-call-btn")!;
const targetFpInput = document.getElementById("target-fp") as HTMLInputElement;
const callBtn = document.getElementById("call-btn") as HTMLButtonElement;
const callStatusText = document.getElementById("call-status-text")!;

let currentCallMode = "room";

modeRoom.addEventListener("click", () => {
  currentCallMode = "room";
  modeRoom.classList.add("active");
  modeDirect.classList.remove("active");
  roomModeDiv.classList.remove("hidden");
  directModeDiv.classList.add("hidden");
  // Show room/alias inputs
  (document.querySelector('label:has(#room)') as HTMLElement)?.classList.remove("hidden");
  (document.querySelector('label:has(#alias)') as HTMLElement)?.classList.remove("hidden");
});

modeDirect.addEventListener("click", () => {
  currentCallMode = "direct";
  modeDirect.classList.add("active");
  modeRoom.classList.remove("active");
  directModeDiv.classList.remove("hidden");
  roomModeDiv.classList.add("hidden");
  // Hide room input, keep alias
  (document.querySelector('label:has(#room)') as HTMLElement)?.classList.add("hidden");
});

registerBtn.addEventListener("click", async () => {
  const relay = getSelectedRelay();
  if (!relay) { connectError.textContent = "No relay selected"; return; }
  registerBtn.disabled = true;
  registerBtn.textContent = "Registering...";
  try {
    const fp = await invoke<string>("register_signal", { relay: relay.address });
    registerBtn.classList.add("hidden");
    directRegistered.classList.remove("hidden");
    callStatusText.textContent = `Your fingerprint: ${fp}`;
  } catch (e: any) {
    connectError.textContent = String(e);
    registerBtn.disabled = false;
    registerBtn.textContent = "Register on Relay";
  }
});

callBtn.addEventListener("click", async () => {
  const target = targetFpInput.value.trim();
  if (!target) return;
  callStatusText.textContent = "Calling...";
  try {
    await invoke("place_call", { targetFp: target });
  } catch (e: any) {
    callStatusText.textContent = `Error: ${e}`;
  }
});

acceptCallBtn.addEventListener("click", async () => {
  const status = await invoke<any>("get_signal_status");
  if (status.incoming_call_id) {
    await invoke("answer_call", { callId: status.incoming_call_id, mode: 2 });
    incomingCallPanel.classList.add("hidden");
  }
});

rejectCallBtn.addEventListener("click", async () => {
  const status = await invoke<any>("get_signal_status");
  if (status.incoming_call_id) {
    await invoke("answer_call", { callId: status.incoming_call_id, mode: 0 });
    incomingCallPanel.classList.add("hidden");
  }
});

// Listen for signal events from Rust backend
listen("signal-event", (event: any) => {
  const data = event.payload;
  switch (data.type) {
    case "ringing":
      callStatusText.textContent = "🔔 Ringing...";
      break;
    case "incoming":
      incomingCallPanel.classList.remove("hidden");
      incomingCaller.textContent = `From: ${data.caller_alias || data.caller_fp?.substring(0, 16) || "unknown"}`;
      break;
    case "answered":
      callStatusText.textContent = `Call answered (${data.mode})`;
      break;
    case "setup":
      callStatusText.textContent = "Connecting to media...";
      // Auto-connect to the call room
      (async () => {
        try {
          await invoke("connect", {
            relay: data.relay_addr,
            room: data.room,
            alias: aliasInput.value,
            osAec: osAecCheckbox.checked,
            quality: loadSettings().quality || "auto",
          });
          showCallScreen();
        } catch (e: any) {
          callStatusText.textContent = `Media connect failed: ${e}`;
        }
      })();
      break;
    case "hangup":
      callStatusText.textContent = "";
      incomingCallPanel.classList.add("hidden");
      break;
  }
});
