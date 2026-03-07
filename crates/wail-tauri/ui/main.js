const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// DOM elements
const firstLaunchScreen = document.getElementById('first-launch-screen');
const firstLaunchForm = document.getElementById('first-launch-form');
const firstLaunchNameInput = document.getElementById('first-launch-name');
const joinScreen = document.getElementById('join-screen');
const sessionScreen = document.getElementById('session-screen');
const joinForm = document.getElementById('join-form');
const joinBtn = document.getElementById('join-btn');
const joinError = document.getElementById('join-error');
const disconnectBtn = document.getElementById('disconnect-btn');
const sessionError = document.getElementById('session-error');
const setBpmBtn = document.getElementById('set-bpm-btn');
const toggleTestToneBtn = document.getElementById('toggle-test-tone-btn');
const settingsBtn = document.getElementById('settings-btn');
const settingsPanel = document.getElementById('settings-panel');
const settingsCloseBtn = document.getElementById('settings-close-btn');
const settingsForm = document.getElementById('settings-form');
const settingsDisplayNameInput = document.getElementById('settings-display-name');
const settingsTelemetryCheckbox = document.getElementById('settings-telemetry');
const settingsLogSharingCheckbox = document.getElementById('settings-log-sharing');
const settingsRememberCheckbox = document.getElementById('settings-remember');

// Version label
window.__TAURI__.app.getVersion().then(v => {
  document.getElementById('version-label').textContent = 'v' + v;
});

// Check for plugin install errors on load
invoke('get_plugin_install_errors').then(errors => {
  if (errors.length === 0) return;
  const modal = document.getElementById('plugin-error-modal');
  const list = document.getElementById('plugin-error-list');
  list.innerHTML = errors.map(e => `<li>${escapeHtml(e)}</li>`).join('');
  modal.style.display = 'flex';
}).catch(() => {});

document.getElementById('plugin-error-close-btn').addEventListener('click', () => {
  document.getElementById('plugin-error-modal').style.display = 'none';
});
document.getElementById('plugin-error-ok-btn').addEventListener('click', () => {
  document.getElementById('plugin-error-modal').style.display = 'none';
});

// State
let unlisten = [];
let testToneEnabled = false;
let roomRefreshTimer = null;

// --- Display Name Storage ---
const DISPLAY_NAME_KEY = 'wail-display-name';
const TELEMETRY_KEY = 'wail-telemetry';
const LOG_SHARING_KEY = 'wail-log-sharing';
const REMEMBER_KEY = 'wail-remember';

function getDisplayName() {
  return localStorage.getItem(DISPLAY_NAME_KEY) || '';
}

function saveDisplayName(name) {
  localStorage.setItem(DISPLAY_NAME_KEY, name);
}

function getTelemetryEnabled() {
  const val = localStorage.getItem(TELEMETRY_KEY);
  return val === null ? true : val === 'true';
}

function saveTelemetryEnabled(enabled) {
  localStorage.setItem(TELEMETRY_KEY, enabled ? 'true' : 'false');
}

function getLogSharingEnabled() {
  const val = localStorage.getItem(LOG_SHARING_KEY);
  return val === 'true';
}

function saveLogSharingEnabled(enabled) {
  localStorage.setItem(LOG_SHARING_KEY, enabled ? 'true' : 'false');
}

function getRememberEnabled() {
  const val = localStorage.getItem(REMEMBER_KEY);
  return val === null ? true : val === 'true';
}

function saveRememberEnabled(enabled) {
  localStorage.setItem(REMEMBER_KEY, enabled ? 'true' : 'false');
}

// --- Remember settings ---
const STORAGE_KEY = 'wail-settings';
const rememberFields = ['room', 'password', 'bars', 'quantum', 'ipc-port', 'test-tone', 'recording-enabled', 'recording-dir', 'recording-stems', 'recording-retention'];

function loadSettings() {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (!saved) return;
    const settings = JSON.parse(saved);
    for (const id of rememberFields) {
      if (settings[id] != null) {
        const el = document.getElementById(id);
        if (el.type === 'checkbox') {
          el.checked = settings[id];
        } else {
          el.value = settings[id];
        }
      }
    }
  } catch (_) {}
}

function saveSettings() {
  if (!getRememberEnabled()) {
    localStorage.removeItem(STORAGE_KEY);
    return;
  }
  const settings = {};
  for (const id of rememberFields) {
    const el = document.getElementById(id);
    settings[id] = el.type === 'checkbox' ? el.checked : el.value;
  }
  localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}

function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

loadSettings();

// Restore recording options visibility after settings load
if (document.getElementById('recording-enabled').checked) {
  document.getElementById('recording-options').style.display = '';
}

// --- First Launch Detection ---
function showFirstLaunch() {
  firstLaunchScreen.style.display = 'flex';
  joinScreen.style.display = 'none';
  firstLaunchNameInput.focus();
}

function showJoinScreen() {
  firstLaunchScreen.style.display = 'none';
  joinScreen.style.display = '';
}

// On page load, check if display name is set
if (!getDisplayName()) {
  showFirstLaunch();
} else {
  showJoinScreen();
}

// First launch form submit
firstLaunchForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const name = firstLaunchNameInput.value.trim();
  if (name) {
    saveDisplayName(name);
    showJoinScreen();
  }
});

// --- Join screen tab switching ---
const tabJoinBtn = document.getElementById('tab-join');
const tabPublicBtn = document.getElementById('tab-public');
const tabJoinContent = document.getElementById('tab-join-content');
const tabPublicContent = document.getElementById('tab-public-content');

tabJoinBtn.addEventListener('click', () => {
  tabJoinBtn.classList.add('active');
  tabPublicBtn.classList.remove('active');
  tabJoinContent.style.display = '';
  tabPublicContent.style.display = 'none';
  stopRoomRefresh();
});

tabPublicBtn.addEventListener('click', () => {
  tabPublicBtn.classList.add('active');
  tabJoinBtn.classList.remove('active');
  tabJoinContent.style.display = 'none';
  tabPublicContent.style.display = '';
  fetchPublicRooms();
  startRoomRefresh();
});

function startRoomRefresh() {
  stopRoomRefresh();
  roomRefreshTimer = setInterval(fetchPublicRooms, 10000);
}

function stopRoomRefresh() {
  if (roomRefreshTimer) {
    clearInterval(roomRefreshTimer);
    roomRefreshTimer = null;
  }
}

async function fetchPublicRooms() {
  try {
    const rooms = await invoke('list_public_rooms');
    renderPublicRooms(rooms);
  } catch (err) {
    document.getElementById('public-rooms-list').innerHTML =
      `<span class="empty">Failed to load: ${escapeHtml(String(err))}</span>`;
  }
}

function renderPublicRooms(rooms) {
  const list = document.getElementById('public-rooms-list');
  if (rooms.length === 0) {
    list.innerHTML = '<span class="empty">No public rooms available</span>';
    return;
  }
  list.innerHTML = rooms.map(r => {
    const bpm = r.bpm ? `${r.bpm.toFixed(0)} BPM` : '-- BPM';
    const names = r.display_names.filter(Boolean).join(', ') || 'anonymous';
    return `<div class="room-card">
      <div class="room-info">
        <span class="room-name">${escapeHtml(r.room)}</span>
        <span class="room-meta">${r.peer_count} peer${r.peer_count !== 1 ? 's' : ''} &middot; ${bpm} &middot; ${escapeHtml(names)}</span>
      </div>
      <button type="button" data-room="${escapeHtml(r.room)}">Join</button>
    </div>`;
  }).join('');

  // Attach click handlers
  list.querySelectorAll('.room-card button').forEach(btn => {
    btn.addEventListener('click', () => joinPublicRoom(btn.dataset.room));
  });
}

async function joinPublicRoom(room) {
  const params = {
    room: room,
    password: null,
    displayName: getDisplayName(),
    bpm: 120.0,
    bars: parseInt(document.getElementById('bars').value),
    quantum: parseFloat(document.getElementById('quantum').value),
    ipcPort: parseInt(document.getElementById('ipc-port').value),
    testTone: document.getElementById('test-tone').checked,
    recordingEnabled: document.getElementById('recording-enabled').checked,
    recordingDirectory: document.getElementById('recording-dir').value || null,
    recordingStems: document.getElementById('recording-stems').checked,
    recordingRetentionDays: parseInt(document.getElementById('recording-retention').value) || 30,
  };
  try {
    const result = await invoke('join_room', params);
    saveSettings();
    stopRoomRefresh();
    showSession(result.room);
    setupListeners();
  } catch (err) {
    showError(joinError, err);
  }
}

document.getElementById('refresh-rooms-btn').addEventListener('click', fetchPublicRooms);

// --- Recording options toggle ---
document.getElementById('recording-enabled').addEventListener('change', (e) => {
  document.getElementById('recording-options').style.display = e.target.checked ? '' : 'none';
});

document.getElementById('browse-recording-dir').addEventListener('click', async () => {
  try {
    const dir = await invoke('get_default_recording_dir');
    document.getElementById('recording-dir').value = dir;
  } catch (err) {
    console.error('Failed to get default recording dir:', err);
  }
});

// Sync telemetry and log sharing state on load
invoke('set_telemetry', { enabled: getTelemetryEnabled() }).catch(() => {});
invoke('set_log_sharing', { enabled: getLogSharingEnabled() }).catch(() => {});

// Populate default recording dir on load
invoke('get_default_recording_dir').then(dir => {
  const el = document.getElementById('recording-dir');
  if (!el.value) el.value = dir;
}).catch(() => {});

// --- Settings Panel ---
settingsBtn.addEventListener('click', () => {
  // Populate settings panel with current values
  settingsDisplayNameInput.value = getDisplayName();
  settingsTelemetryCheckbox.checked = getTelemetryEnabled();
  settingsLogSharingCheckbox.checked = getLogSharingEnabled();
  settingsRememberCheckbox.checked = getRememberEnabled();
  settingsPanel.style.display = 'flex';
});

settingsCloseBtn.addEventListener('click', () => {
  settingsPanel.style.display = 'none';
});

settingsPanel.addEventListener('click', (e) => {
  if (e.target === settingsPanel) {
    settingsPanel.style.display = 'none';
  }
});

settingsForm.addEventListener('submit', (e) => {
  e.preventDefault();
  const name = settingsDisplayNameInput.value.trim();
  if (name) {
    saveDisplayName(name);
  }
  // Save telemetry setting
  const telemetryEnabled = settingsTelemetryCheckbox.checked;
  saveTelemetryEnabled(telemetryEnabled);
  invoke('set_telemetry', { enabled: telemetryEnabled }).catch(() => {});
  // Save log sharing setting
  const logSharingEnabled = settingsLogSharingCheckbox.checked;
  saveLogSharingEnabled(logSharingEnabled);
  invoke('set_log_sharing', { enabled: logSharingEnabled }).catch(() => {});
  // Save remember setting
  const rememberEnabled = settingsRememberCheckbox.checked;
  saveRememberEnabled(rememberEnabled);
  if (rememberEnabled) {
    saveSettings();
  } else {
    localStorage.removeItem(STORAGE_KEY);
  }
  settingsPanel.style.display = 'none';
});

// --- Session screen tab switching ---
const sessionTabSessionBtn = document.getElementById('session-tab-session');
const sessionTabNetworkBtn = document.getElementById('session-tab-network');
const sessionTabSessionContent = document.getElementById('session-tab-session-content');
const sessionTabNetworkContent = document.getElementById('session-tab-network-content');

sessionTabSessionBtn.addEventListener('click', () => {
  sessionTabSessionBtn.classList.add('active');
  sessionTabNetworkBtn.classList.remove('active');
  sessionTabSessionContent.style.display = '';
  sessionTabNetworkContent.style.display = 'none';
});

sessionTabNetworkBtn.addEventListener('click', () => {
  sessionTabNetworkBtn.classList.add('active');
  sessionTabSessionBtn.classList.remove('active');
  sessionTabSessionContent.style.display = 'none';
  sessionTabNetworkContent.style.display = '';
});

function showJoin() {
  firstLaunchScreen.style.display = 'none';
  joinScreen.style.display = '';
  sessionScreen.style.display = 'none';
  joinError.style.display = 'none';
  joinBtn.disabled = false;
  joinBtn.textContent = 'Join Room';
  // Reset session tabs to Session on leave
  sessionTabSessionBtn.classList.add('active');
  sessionTabNetworkBtn.classList.remove('active');
  sessionTabSessionContent.style.display = '';
  sessionTabNetworkContent.style.display = 'none';
  cleanup();
}

function showSession(room) {
  joinScreen.style.display = 'none';
  sessionScreen.style.display = '';
  sessionError.style.display = 'none';
  clearLog();
  document.getElementById('session-room').textContent = room;
  document.getElementById('peer-list').innerHTML = '<span class="empty">No peers connected</span>';
  document.getElementById('session-audio').textContent = '0 sent / 0 recv';
  document.getElementById('session-audio-bytes').textContent = '0 B sent / 0 B recv';
  document.getElementById('session-plugin').textContent = 'disconnected';
  document.getElementById('session-plugin').className = '';
  document.getElementById('session-link-peers').textContent = '0';
  document.getElementById('session-interval').textContent = '-';
  testToneEnabled = document.getElementById('test-tone').checked;
  updateTestToneUI();
  document.getElementById('recording-stat').style.display =
    document.getElementById('recording-enabled').checked ? '' : 'none';
}

function updateTestToneUI() {
  document.getElementById('session-test-tone').textContent = testToneEnabled ? 'ON' : 'OFF';
  document.getElementById('session-test-tone').className = testToneEnabled ? 'connected' : '';
  toggleTestToneBtn.textContent = testToneEnabled ? 'Disable' : 'Enable';
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
    room: document.getElementById('room').value,
    password: document.getElementById('password').value || null,
    displayName: getDisplayName(),
    bpm: 120.0,
    bars: parseInt(document.getElementById('bars').value),
    quantum: parseFloat(document.getElementById('quantum').value),
    ipcPort: parseInt(document.getElementById('ipc-port').value),
    testTone: document.getElementById('test-tone').checked,
    recordingEnabled: document.getElementById('recording-enabled').checked,
    recordingDirectory: document.getElementById('recording-dir').value || null,
    recordingStems: document.getElementById('recording-stems').checked,
    recordingRetentionDays: parseInt(document.getElementById('recording-retention').value) || 30,
  };

  try {
    const result = await invoke('join_room', params);
    saveSettings();
    showSession(result.room);
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

// --- Test Tone Toggle ---
toggleTestToneBtn.addEventListener('click', async () => {
  testToneEnabled = !testToneEnabled;
  try {
    await invoke('set_test_tone', { enabled: testToneEnabled });
  } catch (err) {
    console.error('Test tone toggle error:', err);
    testToneEnabled = !testToneEnabled; // revert on error
  }
  updateTestToneUI();
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
    document.getElementById('session-link-peers').textContent = s.link_peers;
    document.getElementById('link-no-peers-warning').style.display =
      (s.link_peers === 0 && s.plugin_connected) ? '' : 'none';
    document.getElementById('session-audio').textContent =
      `${s.audio_sent} sent / ${s.audio_recv} recv`;
    document.getElementById('session-audio-bytes').textContent =
      `${formatBytes(s.audio_bytes_sent)} sent / ${formatBytes(s.audio_bytes_recv)} recv`;
    document.getElementById('session-interval').textContent = `${s.interval_bars} bars`;
    document.getElementById('session-plugin').textContent =
      s.plugin_connected ? 'connected' : 'disconnected';
    document.getElementById('session-plugin').className =
      s.plugin_connected ? 'connected' : '';

    // Sync test tone state
    testToneEnabled = s.test_tone_enabled;
    updateTestToneUI();

    // Update recording status
    if (s.recording) {
      document.getElementById('recording-stat').style.display = '';
      const mb = (s.recording_size_bytes / (1024 * 1024)).toFixed(1);
      document.getElementById('recording-size').textContent = `${mb} MB`;
    }

    // Update slot list (slot-centric view)
    const slotList = document.getElementById('peer-list');
    const slots = (s.slots || []).slice().sort((a, b) => a.slot - b.slot);
    if (slots.length === 0) {
      slotList.innerHTML = '<span class="empty">No peers connected</span>';
    } else {
      slotList.innerHTML = slots.map(sl => {
        const name = sl.display_name
          ? `${escapeHtml(sl.display_name)} (${escapeHtml(sl.short_id)})`
          : escapeHtml(sl.short_id);
        const rtt = sl.rtt_ms != null ? `${sl.rtt_ms.toFixed(0)}ms` : '...';
        const status = sl.status || 'connecting';
        const statusClass = `peer-status status-${status}`;
        return `<div class="peer-item">
          <span class="peer-name"><span class="peer-slot">Slot ${sl.slot}</span>${name}</span>
          <span class="${statusClass}">${escapeHtml(status)}</span>
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
    const p = event.payload;
    addLogEntry(p.level, p.message, p.peer_name || p.peer_id || null);
  }));

  unlisten.push(await listen('peers:network', (event) => {
    const peers = event.payload.peers;
    const tbody = document.getElementById('network-table-body');
    if (peers.length === 0) {
      tbody.innerHTML = '<tr><td colspan="7" class="empty">No peers connected</td></tr>';
      return;
    }
    tbody.innerHTML = peers.map(p => {
      const name = p.display_name
        ? escapeHtml(p.display_name)
        : escapeHtml(p.peer_id.slice(0, 8));
      const slot = p.slot != null ? `Slot ${p.slot}` : '-';
      const rtt = p.rtt_ms != null ? `${p.rtt_ms.toFixed(0)}ms` : '-';
      return `<tr>
        <td>${name}</td>
        <td>${slot}</td>
        <td class="net-state net-${escapeHtml(p.ice_state)}">${escapeHtml(p.ice_state)}</td>
        <td class="net-state net-${escapeHtml(p.dc_sync_state)}">${escapeHtml(p.dc_sync_state)}</td>
        <td class="net-state net-${escapeHtml(p.dc_audio_state)}">${escapeHtml(p.dc_audio_state)}</td>
        <td>${rtt}</td>
        <td>${p.audio_recv}</td>
      </tr>`;
    }).join('');
  }));
}

// --- Log panel ---
let logEntries = [];
const MAX_LOG_ENTRIES = 200;

function addLogEntry(level, message, peerLabel) {
  const time = new Date().toLocaleTimeString();
  logEntries.push({ time, level, message });
  if (logEntries.length > MAX_LOG_ENTRIES) {
    logEntries.shift();
  }

  const logList = document.getElementById('log-list');
  const entry = document.createElement('div');
  entry.className = `log-entry ${level}${peerLabel ? ' peer-log' : ''}`;
  const peerPrefix = peerLabel ? `<span class="log-peer">[${escapeHtml(peerLabel)}]</span> ` : '';
  entry.innerHTML = `<span class="log-time">${time}</span>${peerPrefix}${escapeHtml(message)}`;
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

