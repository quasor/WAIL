const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// DOM elements
const joinScreen = document.getElementById('join-screen');
const sessionScreen = document.getElementById('session-screen');
const joinForm = document.getElementById('join-form');
const joinBtn = document.getElementById('join-btn');
const joinError = document.getElementById('join-error');
const disconnectBtn = document.getElementById('disconnect-btn');
const sessionError = document.getElementById('session-error');
const setBpmBtn = document.getElementById('set-bpm-btn');
const installPluginsBtn = document.getElementById('install-plugins-btn');
const pluginStatus = document.getElementById('plugin-status');

// State
let unlisten = [];

// --- Remember settings ---
const STORAGE_KEY = 'wail-settings';
const rememberFields = ['room', 'password', 'display-name', 'bpm', 'server', 'bars', 'quantum', 'ipc-port'];

function loadSettings() {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (!saved) return;
    const settings = JSON.parse(saved);
    for (const id of rememberFields) {
      if (settings[id] != null) {
        document.getElementById(id).value = settings[id];
      }
    }
    document.getElementById('remember').checked = true;
  } catch (_) {}
}

function saveSettings() {
  if (!document.getElementById('remember').checked) {
    localStorage.removeItem(STORAGE_KEY);
    return;
  }
  const settings = {};
  for (const id of rememberFields) {
    settings[id] = document.getElementById(id).value;
  }
  localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}

loadSettings();

document.getElementById('remember').addEventListener('change', () => {
  if (!document.getElementById('remember').checked) {
    localStorage.removeItem(STORAGE_KEY);
  }
});

function showJoin() {
  joinScreen.style.display = '';
  sessionScreen.style.display = 'none';
  joinError.style.display = 'none';
  joinBtn.disabled = false;
  joinBtn.textContent = 'Join Room';
  cleanup();
}

function showSession(room, bpm) {
  joinScreen.style.display = 'none';
  sessionScreen.style.display = '';
  sessionError.style.display = 'none';
  clearLog();
  document.getElementById('session-room').textContent = room;
  document.getElementById('session-bpm').value = bpm;
  document.getElementById('peer-list').innerHTML = '<span class="empty">No peers connected</span>';
  document.getElementById('session-audio').textContent = '0 sent / 0 recv';
  document.getElementById('session-plugin').textContent = 'disconnected';
  document.getElementById('session-plugin').className = '';
  document.getElementById('session-link-peers').textContent = '0';
  document.getElementById('session-position').textContent = '-';
  document.getElementById('session-interval').textContent = '-';
}

function showError(el, msg) {
  el.textContent = msg;
  el.style.display = '';
}

function cleanup() {
  unlisten.forEach(fn => fn());
  unlisten = [];
}

// --- Join ---
joinForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  joinError.style.display = 'none';
  joinBtn.disabled = true;
  joinBtn.textContent = 'Connecting...';

  const params = {
    server: document.getElementById('server').value,
    room: document.getElementById('room').value,
    password: document.getElementById('password').value,
    displayName: document.getElementById('display-name').value || null,
    bpm: parseFloat(document.getElementById('bpm').value),
    bars: parseInt(document.getElementById('bars').value),
    quantum: parseFloat(document.getElementById('quantum').value),
    ipcPort: parseInt(document.getElementById('ipc-port').value),
  };

  try {
    const result = await invoke('join_room', params);
    saveSettings();
    showSession(result.room, result.bpm);
    setupListeners();
  } catch (err) {
    showError(joinError, err);
    joinBtn.disabled = false;
    joinBtn.textContent = 'Join Room';
  }
});

// --- Disconnect ---
disconnectBtn.addEventListener('click', async () => {
  try {
    await invoke('disconnect');
  } catch (err) {
    console.error('Disconnect error:', err);
  }
  showJoin();
});

// --- Set BPM ---
setBpmBtn.addEventListener('click', async () => {
  const bpm = parseFloat(document.getElementById('session-bpm').value);
  if (isNaN(bpm) || bpm < 20 || bpm > 999) return;
  try {
    await invoke('change_bpm', { bpm });
  } catch (err) {
    console.error('BPM change error:', err);
  }
});

document.getElementById('session-bpm').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    setBpmBtn.click();
  }
});

// --- Event Listeners ---
async function setupListeners() {
  cleanup();

  unlisten.push(await listen('status:update', (event) => {
    const s = event.payload;
    const bpmInput = document.getElementById('session-bpm');
    if (document.activeElement !== bpmInput) {
      bpmInput.value = s.bpm.toFixed(1);
    }
    const quantum = s.interval_bars > 0 ? 4 : 4; // beats per bar
    const bar = Math.floor(s.beat / quantum) + 1;
    const beatInBar = Math.floor(s.phase) + 1;
    document.getElementById('session-position').textContent = `Bar ${bar} · Beat ${beatInBar}`;
    document.getElementById('session-link-peers').textContent = s.link_peers;
    document.getElementById('session-audio').textContent =
      `${s.audio_sent} sent / ${s.audio_recv} recv`;
    document.getElementById('session-interval').textContent = `${s.interval_bars} bars`;
    document.getElementById('session-plugin').textContent =
      s.plugin_connected ? 'connected' : 'disconnected';
    document.getElementById('session-plugin').className =
      s.plugin_connected ? 'connected' : '';

    // Update peer list
    const peerList = document.getElementById('peer-list');
    if (s.peers.length === 0) {
      peerList.innerHTML = '<span class="empty">No peers connected</span>';
    } else {
      peerList.innerHTML = s.peers.map(p => {
        const name = p.display_name || p.peer_id.slice(0, 6);
        const rtt = p.rtt_ms != null ? `${p.rtt_ms.toFixed(0)}ms` : '...';
        return `<div class="peer-item">
          <span class="peer-name">${escapeHtml(name)}</span>
          <span class="peer-rtt">${rtt}</span>
        </div>`;
      }).join('');
    }
  }));

  unlisten.push(await listen('tempo:changed', (event) => {
    document.getElementById('session-bpm').value = event.payload.bpm.toFixed(1);
  }));

  unlisten.push(await listen('session:error', (event) => {
    showError(sessionError, event.payload.message);
  }));

  unlisten.push(await listen('session:ended', () => {
    showJoin();
  }));

  unlisten.push(await listen('plugin:connected', () => {
    document.getElementById('session-plugin').textContent = 'connected';
    document.getElementById('session-plugin').className = 'connected';
  }));

  unlisten.push(await listen('plugin:disconnected', () => {
    document.getElementById('session-plugin').textContent = 'disconnected';
    document.getElementById('session-plugin').className = '';
  }));

  unlisten.push(await listen('log:entry', (event) => {
    addLogEntry(event.payload.level, event.payload.message);
  }));
}

// --- Log panel ---
let logEntries = [];
const MAX_LOG_ENTRIES = 200;

function addLogEntry(level, message) {
  const time = new Date().toLocaleTimeString();
  logEntries.push({ time, level, message });
  if (logEntries.length > MAX_LOG_ENTRIES) {
    logEntries.shift();
  }

  const logList = document.getElementById('log-list');
  const entry = document.createElement('div');
  entry.className = `log-entry ${level}`;
  entry.innerHTML = `<span class="log-time">${time}</span>${escapeHtml(message)}`;
  logList.appendChild(entry);
  logList.scrollTop = logList.scrollHeight;

  // Trim DOM to match cap
  while (logList.children.length > MAX_LOG_ENTRIES) {
    logList.removeChild(logList.firstChild);
  }

  // Update badge
  const badge = document.getElementById('log-count');
  badge.textContent = logEntries.length;
  const hasErrors = logEntries.some(e => e.level === 'error');
  const hasWarnings = logEntries.some(e => e.level === 'warn');
  badge.className = 'log-badge' +
    (hasErrors ? ' has-errors' : hasWarnings ? ' has-warnings' : '');
}

function clearLog() {
  logEntries = [];
  document.getElementById('log-list').innerHTML = '';
  const badge = document.getElementById('log-count');
  badge.textContent = '0';
  badge.className = 'log-badge';
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

// --- Plugin Install ---
async function checkPlugins() {
  try {
    const status = await invoke('check_plugins_installed');
    if (status.clap && status.vst3) {
      pluginStatus.textContent = 'Plugins installed';
      pluginStatus.className = 'connected';
      installPluginsBtn.style.display = 'none';
    } else {
      const missing = [];
      if (!status.clap) missing.push('CLAP');
      if (!status.vst3) missing.push('VST3');
      pluginStatus.textContent = `Missing: ${missing.join(', ')}`;
      installPluginsBtn.style.display = '';
    }
  } catch (err) {
    pluginStatus.textContent = 'Could not check plugin status';
    installPluginsBtn.style.display = 'none';
  }
}

installPluginsBtn.addEventListener('click', async () => {
  installPluginsBtn.disabled = true;
  installPluginsBtn.textContent = 'Installing...';
  try {
    const result = await invoke('install_plugins');
    pluginStatus.textContent = 'Plugins installed';
    pluginStatus.className = 'connected';
    installPluginsBtn.style.display = 'none';
  } catch (err) {
    showError(joinError, `Plugin install failed: ${err}`);
    installPluginsBtn.disabled = false;
    installPluginsBtn.textContent = 'Install Plugins';
  }
});

// Check plugin status on load
checkPlugins();
