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
const sessionBpmInput = document.getElementById('session-bpm');
const testToneSelect = document.getElementById('test-tone-select');
const settingsBtn = document.getElementById('settings-btn');
const settingsPanel = document.getElementById('settings-panel');
const settingsCloseBtn = document.getElementById('settings-close-btn');
const settingsForm = document.getElementById('settings-form');
const settingsDisplayNameInput = document.getElementById('settings-display-name');
const settingsTelemetryCheckbox = document.getElementById('settings-telemetry');
const settingsLogSharingCheckbox = document.getElementById('settings-log-sharing');
const settingsRememberCheckbox = document.getElementById('settings-remember');
const chatInput = document.getElementById('chat-input');
const chatSendBtn = document.getElementById('chat-send-btn');
const chatMessages = document.getElementById('chat-messages');

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

// --- Room Name Generator ---
// Dictionary 1: synthesis techniques, sound qualities, processing descriptors
const ROOM_MODIFIERS = [
  // Synthesis methods
  "Analog", "Digital", "Modular", "Granular", "Wavetable", "Spectral",
  "FM", "Additive", "Subtractive", "Physical", "Hybrid", "Generative",
  "Algorithmic", "Euclidean", "Stochastic", "Polyrhythmic", "Microtonal",
  "Bitcrushed", "Saturated", "Filtered",
  // Sound texture
  "Resonant", "Distorted", "Lush", "Bright", "Dark", "Warm", "Muted",
  "Punchy", "Dense", "Sparse", "Rich", "Thick", "Heavy", "Massive",
  "Delicate", "Hollow", "Crisp", "Tight", "Deep", "Raw",
  // Material texture
  "Fuzzy", "Gritty", "Silky", "Airy", "Glassy", "Crystalline", "Murky",
  "Grainy", "Polished", "Rough", "Smooth", "Sharp", "Jagged", "Fractured",
  "Porous", "Metallic", "Wooden", "Translucent", "Sheer", "Brittle",
  // Musical character
  "Harmonic", "Dissonant", "Chromatic", "Pentatonic", "Melodic", "Rhythmic",
  "Syncopated", "Percussive", "Polyphonic", "Monophonic", "Atmospheric",
  "Ambient", "Cinematic", "Minimal", "Chaotic", "Ethereal", "Dynamic",
  "Textural", "Orchestral", "Improvised",
  // FX and processing
  "Compressed", "Reverberant", "Delayed", "Modulated", "Phased", "Flanged",
  "Chorused", "Trembling", "Chopped", "Detuned", "Looped", "Sampled",
  "Processed", "Quantized", "Shuffled", "Swung", "Glitchy", "Warped",
  "Folded", "Stretched",
  // Motion and energy
  "Driving", "Pulsing", "Swirling", "Drifting", "Floating", "Swelling",
  "Soaring", "Rising", "Fading", "Cascading", "Spiraling", "Flowing",
  "Streaming", "Weaving", "Twisting", "Spinning", "Rolling", "Rumbling",
  "Surging", "Plunging",
  // Sonic texture
  "Crackling", "Humming", "Buzzing", "Droning", "Shimmering", "Ringing",
  "Echoing", "Chiming", "Hissing", "Growling", "Thundering", "Whispering",
  "Roaring", "Murmuring", "Sizzling", "Howling", "Tolling", "Clicking",
  "Snapping", "Sputtering",
  // Elemental and environmental
  "Liquid", "Frozen", "Glowing", "Burning", "Electric", "Acoustic",
  "Industrial", "Celestial", "Subterranean", "Volcanic", "Arctic", "Tropical",
  "Cosmic", "Galactic", "Lunar", "Solar", "Oceanic", "Glacial", "Abyssal",
  "Prismatic",
  // Vibe and mood
  "Serene", "Hypnotic", "Turbulent", "Aggressive", "Progressive",
  "Experimental", "Retro", "Vintage", "Futuristic", "Adaptive", "Radiant",
  "Shadowed", "Vibrant", "Fragile", "Robust", "Fluid", "Piercing", "Subtle",
  "Stark", "Tender",
  // Structural and abstract
  "Fractal", "Recursive", "Divergent", "Convergent", "Parallel", "Inverted",
  "Mirrored", "Nested", "Branching", "Tangled", "Cyclic", "Iterative",
  "Stacked", "Scattered", "Clustered", "Dispersed", "Morphing", "Evolving",
  "Unraveling", "Awakening",
];
// Dictionary 2: instruments, sound sources, and audio components
const ROOM_ELEMENTS = [
  // Synth voices and concepts
  "Synth", "Bass", "Arp", "Pad", "Lead", "Drone", "Pulse", "Chord",
  "Riff", "Patch", "Oscillator", "Filter", "Resonance", "LFO", "Envelope",
  "Sequencer", "Arpeggio", "Waveform", "Feedback", "Glitch",
  // String instruments
  "Guitar", "Piano", "Violin", "Cello", "Harp", "Lute", "Mandolin",
  "Banjo", "Sitar", "Koto", "Dulcimer", "Zither", "Ukulele", "Balalaika",
  "Bouzouki", "Oud", "Shamisen", "Erhu", "Guqin", "Theorbo",
  // Keys and organ family
  "Organ", "Harpsichord", "Clavichord", "Rhodes", "Wurlitzer", "Clavinet",
  "Mellotron", "Ondes", "Cristal", "Celesta",
  // Wind instruments
  "Flute", "Piccolo", "Oboe", "Clarinet", "Bassoon", "Saxophone", "Trumpet",
  "Trombone", "Tuba", "Flugelhorn", "Cornet", "Horn", "Recorder", "Ocarina",
  "Harmonica", "Accordion", "Bagpipes", "Didgeridoo", "Duduk", "Shakuhachi",
  // Tuned percussion
  "Marimba", "Vibraphone", "Xylophone", "Glockenspiel", "Theremin",
  "Kalimba", "Mbira", "Balafon", "Handpan", "Steeldrum",
  // Drums and untuned percussion
  "Kick", "Snare", "Hat", "Tom", "Clap", "Cymbal", "Cowbell", "Bongo",
  "Conga", "Tabla", "Djembe", "Cajon", "Maracas", "Tambourine", "Gong",
  "Woodblock", "Triangle", "Clave", "Rimshot", "Shaker",
  // FX units and processors
  "Reverb", "Delay", "Chorus", "Flanger", "Phaser", "Distortion",
  "Overdrive", "Bitcrusher", "Ringmod", "Waveshaper", "Limiter",
  "Compressor", "Clipper", "Saturator", "Expander", "Vocoder", "Sampler",
  "Transient", "Preamp", "Sidechain",
  // Modular and studio gear
  "VCO", "VCF", "VCA", "Mixer", "Attenuator", "Quantizer", "Module",
  "Rack", "Trigger", "Clock", "MIDI", "Keyboard", "Fader", "Crossfader",
  "Interface", "Controller", "Patchbay", "Bus", "Matrix", "Oscilloscope",
  // Vocal and choral
  "Voice", "Choir", "Vox", "Whisper", "Breath", "Scat", "Beatbox",
  "Chant", "Hymn", "Yodel", "Canon", "Fugue", "Round", "Ballad",
  "Lullaby", "Dirge", "Shanty", "Spiritual", "Mantra", "Call",
  // Sound and signal concepts
  "Noise", "Signal", "Frequency", "Amplitude", "Spectrum", "Overtone",
  "Undertone", "Resonator", "Transducer", "Exciter",
  // Music theory
  "Tone", "Note", "Pitch", "Timbre", "Scale", "Mode", "Harmony", "Melody",
  "Rhythm", "Cadence", "Phrase", "Motif", "Theme", "Ostinato", "Groove",
  "Beat", "Loop", "Sample", "Measure", "Tempo",
  // Acoustic and natural sound sources
  "Bell", "Chime", "Reed", "Bow", "Membrane", "Mallet", "Bowl", "Spring",
  "Wire", "Fork",
];
// Dictionary 3: venues, events, states, and places
const ROOM_VENUES = [
  // Music venues and events
  "Jam", "Session", "Lounge", "Studio", "Stage", "Gig", "Show",
  "Concert", "Festival", "Showcase", "Performance", "Exhibition", "Revue",
  "Recital", "Rehearsal", "Residency", "Soundcheck", "Opener", "Encore", "Set",
  // Abstract spaces and structures
  "Chamber", "Vault", "Nexus", "Temple", "Asylum", "Haven", "Forge",
  "Portal", "Labyrinth", "Sanctum", "Vortex", "Core", "Hub", "Crypt",
  "Tower", "Keep", "Spire", "Nave", "Atrium", "Rotunda",
  // Celestial and cosmic
  "Orbit", "Eclipse", "Nebula", "Cosmos", "Horizon", "Zenith", "Meridian",
  "Solstice", "Equinox", "Singularity", "Nova", "Pulsar", "Quasar",
  "Galaxy", "Aphelion", "Perihelion", "Transit", "Conjunction", "Nadir", "Apex",
  // Earthly geography and nature
  "Canyon", "Cavern", "Grotto", "Forest", "Meadow", "Tundra", "Delta",
  "Reef", "Glacier", "Volcano", "Mesa", "Basin", "Archipelago", "Fjord",
  "Estuary", "Bayou", "Savanna", "Plateau", "Atoll", "Peninsula",
  // Architectural
  "Cathedral", "Arena", "Amphitheater", "Citadel", "Fortress", "Monastery",
  "Abbey", "Basilica", "Pavilion", "Gallery", "Cloister", "Colonnade",
  "Arcade", "Stronghold", "Rampart", "Bastion", "Outpost", "Station",
  "Terminal", "Junction",
  // Mythological and fantastical
  "Realm", "Domain", "Lair", "Refuge", "Hideout", "Threshold", "Passage",
  "Gateway", "Crossroads", "Waypoint", "Dimension", "Plane", "Stratum",
  "Sector", "Territory", "Province", "Circuit", "Ring", "Spiral", "Field",
  // Water and flow
  "Ocean", "Sea", "Lake", "River", "Spring", "Harbor", "Cove", "Lagoon",
  "Gulf", "Strait", "Surge", "Tide", "Ebb", "Flux", "Current",
  "Stream", "Channel", "Pool", "Cascade", "Torrent",
  // Temporal states and phases
  "Dawn", "Dusk", "Twilight", "Midnight", "Genesis", "Origin", "Terminus",
  "Coda", "Finale", "Prologue", "Epilogue", "Interlude", "Overture",
  "Climax", "Resolution", "Summit", "Drift", "Epoch", "Aeon", "Vesper",
  // Atmosphere and light
  "Aurora", "Prism", "Haze", "Fog", "Mist", "Shadow", "Glow", "Gleam",
  "Shimmer", "Glare", "Blaze", "Flare", "Spark", "Ember", "Ash",
  "Smoke", "Storm", "Thunder", "Lightning", "Rainbow",
  // Institutions and places of learning
  "Legacy", "Vision", "Quest", "Odyssey", "Workshop", "Laboratory",
  "Academy", "Archive", "Conservatory", "Observatory", "Auditorium",
  "Colosseum", "Scriptorium", "Atelier", "Foundry", "Greenhouse",
  "Salon", "Parlor", "Ballroom", "Planetarium",
];
function generateRoomName() {
  const pick = arr => arr[Math.floor(Math.random() * arr.length)];
  return pick(ROOM_MODIFIERS) + pick(ROOM_ELEMENTS) + pick(ROOM_VENUES);
}
document.getElementById('generate-room-btn').addEventListener('click', () => {
  document.getElementById('room').value = generateRoomName();
});

// State
let unlisten = [];
let testToneStream = null; // null or stream index number
let roomRefreshTimer = null;

// Rolling stats window state
const STATS_WINDOW_SIZE = 60; // 60 ticks x 2s = 2 minutes
let statsMode = 'all';        // 'all' or 'recent'
let statusSnapshots = [];
let networkSnapshots = [];
let lastStatusPayload = null;
let lastNetworkPeers = null;

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
const rememberFields = ['room', 'password', 'bars', 'quantum', 'test-tone', 'recording-enabled', 'recording-dir', 'recording-stems', 'recording-retention'];

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
function openSettings() {
  settingsDisplayNameInput.value = getDisplayName();
  settingsTelemetryCheckbox.checked = getTelemetryEnabled();
  settingsLogSharingCheckbox.checked = getLogSharingEnabled();
  settingsRememberCheckbox.checked = getRememberEnabled();
  settingsPanel.style.display = 'flex';
}

settingsBtn.addEventListener('click', openSettings);
document.getElementById('session-settings-btn').addEventListener('click', openSettings);

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
const sessionTabChatBtn = document.getElementById('session-tab-chat');
const sessionTabLogsBtn = document.getElementById('session-tab-logs');
const sessionTabNetworkBtn = document.getElementById('session-tab-network');
const sessionTabDebugBtn = document.getElementById('session-tab-debug');
const sessionTabSessionContent = document.getElementById('session-tab-session-content');
const sessionTabChatContent = document.getElementById('session-tab-chat-content');
const sessionTabLogsContent = document.getElementById('session-tab-logs-content');
const sessionTabNetworkContent = document.getElementById('session-tab-network-content');
const sessionTabDebugContent = document.getElementById('session-tab-debug-content');
const debugCanvas = document.getElementById('debug-interval-canvas');

const SESSION_TABS = [
  { btn: sessionTabSessionBtn, content: sessionTabSessionContent },
  { btn: sessionTabChatBtn,    content: sessionTabChatContent },
  { btn: sessionTabLogsBtn,    content: sessionTabLogsContent },
  { btn: sessionTabNetworkBtn, content: sessionTabNetworkContent },
  { btn: sessionTabDebugBtn,   content: sessionTabDebugContent },
];

function switchSessionTab(activeBtn) {
  SESSION_TABS.forEach(({ btn, content }) => {
    btn.classList.toggle('active', btn === activeBtn);
    content.style.display = btn === activeBtn ? '' : 'none';
  });
  if (activeBtn !== sessionTabDebugBtn) debugSetActive(false);
}

sessionTabSessionBtn.addEventListener('click', () => switchSessionTab(sessionTabSessionBtn));
sessionTabChatBtn.addEventListener('click', () => switchSessionTab(sessionTabChatBtn));
sessionTabLogsBtn.addEventListener('click', () => switchSessionTab(sessionTabLogsBtn));
sessionTabNetworkBtn.addEventListener('click', () => switchSessionTab(sessionTabNetworkBtn));
sessionTabDebugBtn.addEventListener('click', () => {
  switchSessionTab(sessionTabDebugBtn);
  debugSetActive(true);
  requestAnimationFrame(debugRender);
});

function resetStatsWindow() {
  statusSnapshots = [];
  networkSnapshots = [];
  statsMode = 'all';
  lastStatusPayload = null;
  lastNetworkPeers = null;
  document.getElementById('stats-mode-btn').textContent = 'all time';
  document.getElementById('stats-mode-btn-net').textContent = 'all time';
}

function showJoin() {
  firstLaunchScreen.style.display = 'none';
  joinScreen.style.display = '';
  sessionScreen.style.display = 'none';
  joinError.style.display = 'none';
  joinBtn.disabled = false;
  joinBtn.textContent = 'Join Room';
  switchSessionTab(sessionTabSessionBtn);
  resetStatsWindow();
  debugReset();
  cleanup();
}

function showSession(room) {
  joinScreen.style.display = 'none';
  sessionScreen.style.display = '';
  sessionError.style.display = 'none';
  resetStatsWindow();
  clearLog();
  clearChatMessages();
  document.getElementById('session-room').textContent = room;
  document.getElementById('peer-list').innerHTML = '<span class="empty">No peers connected</span>';
  document.getElementById('session-audio').textContent = '0 / 0';
  document.getElementById('session-audio-bytes').textContent = '0 B / 0 B';
  document.getElementById('session-plugin').textContent = 'disconnected';
  document.getElementById('session-plugin').className = 'status-value';
  document.getElementById('session-link-peers').textContent = '0';
  document.getElementById('session-interval').textContent = '-';
  testToneStream = document.getElementById('test-tone').checked ? 0 : null;
  document.getElementById('recording-stat').style.display =
    document.getElementById('recording-enabled').checked ? '' : 'none';
}

function updateTestToneUI() {
  testToneSelect.value = testToneStream != null ? String(testToneStream) : '';
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

// --- Set BPM (on Enter or blur) ---
async function applyBpm() {
  const bpm = parseFloat(sessionBpmInput.value);
  if (isNaN(bpm) || bpm < 20 || bpm > 999) return;
  try {
    await invoke('change_bpm', { bpm });
  } catch (err) {
    console.error('BPM change error:', err);
  }
}

sessionBpmInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    sessionBpmInput.blur();
  }
});

sessionBpmInput.addEventListener('change', applyBpm);

// --- Test Tone Toggle ---
testToneSelect.addEventListener('change', async () => {
  const val = testToneSelect.value;
  const streamIndex = val === '' ? null : parseInt(val, 10);
  try {
    await invoke('set_test_tone', { streamIndex });
    testToneStream = streamIndex;
  } catch (err) {
    console.error('Test tone error:', err);
    updateTestToneUI(); // revert
  }
});

// --- Stats mode toggle click handlers ---
document.getElementById('stats-mode-btn').addEventListener('click', toggleStatsMode);
document.getElementById('stats-mode-btn-net').addEventListener('click', toggleStatsMode);

// --- Chat ---
chatSendBtn.addEventListener('click', sendChatMessage);
chatInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    sendChatMessage();
  }
});

// --- Stats mode toggle ---
function toggleStatsMode() {
  statsMode = statsMode === 'all' ? 'recent' : 'all';
  const label = statsMode === 'all' ? 'all time' : 'last 2 min';
  document.getElementById('stats-mode-btn').textContent = label;
  document.getElementById('stats-mode-btn-net').textContent = label;
  if (lastStatusPayload) renderStatus(lastStatusPayload);
  if (lastNetworkPeers) renderNetwork(lastNetworkPeers);
}

function renderStatus(s) {
  const bpmInput = sessionBpmInput;
  if (document.activeElement !== bpmInput) {
    bpmInput.value = s.bpm.toFixed(1);
  }
  document.getElementById('session-link-peers').textContent = s.link_peers;
  document.getElementById('link-no-peers-warning').style.display =
    (s.link_peers === 0 && s.plugin_connected) ? '' : 'none';

  // Compute display values (windowed or cumulative)
  let sent = s.audio_sent, recv = s.audio_recv;
  let bytesSent = s.audio_bytes_sent, bytesRecv = s.audio_bytes_recv;
  if (statsMode === 'recent' && statusSnapshots.length > 1) {
    const oldest = statusSnapshots[0];
    sent = Math.max(0, s.audio_sent - oldest.audio_sent);
    recv = Math.max(0, s.audio_recv - oldest.audio_recv);
    bytesSent = Math.max(0, s.audio_bytes_sent - oldest.audio_bytes_sent);
    bytesRecv = Math.max(0, s.audio_bytes_recv - oldest.audio_bytes_recv);
  }
  document.getElementById('session-audio').textContent = `${sent} / ${recv}`;
  document.getElementById('session-audio-bytes').textContent =
    `${formatBytes(bytesSent)} / ${formatBytes(bytesRecv)}`;

  document.getElementById('session-interval').textContent = `${s.interval_bars} bar${s.interval_bars !== 1 ? 's' : ''}`;
  document.getElementById('session-plugin').textContent =
    s.plugin_connected ? 'connected' : 'disconnected';
  document.getElementById('session-plugin').className =
    s.plugin_connected ? 'status-value connected' : 'status-value';

  // Sync test tone state
  testToneStream = s.test_tone_stream;
  updateTestToneUI();

  // Update recording status
  if (s.recording) {
    document.getElementById('recording-stat').style.display = '';
    const mb = (s.recording_size_bytes / (1024 * 1024)).toFixed(1);
    document.getElementById('recording-size').textContent = `${mb} MB`;
  }

  // Update slot list (local sends first, then remote slots)
  const slotList = document.getElementById('peer-list');
  const localSends = s.local_sends || [];
  const slots = (s.slots || []).slice().sort((a, b) => a.slot - b.slot);
  if (localSends.length === 0 && slots.length === 0) {
    slotList.innerHTML = '<span class="empty">No peers connected</span>';
  } else {
    // Skip re-rendering local sends if user is editing a stream name
    const isEditingStreamName = slotList.querySelector('.stream-name-input') != null;
    const localHtml = isEditingStreamName ? null : localSends.map(ls => {
      const label = ls.stream_name
        ? escapeHtml(ls.stream_name)
        : (localSends.length > 1 ? `My Send (stream ${ls.stream_index})` : 'My Send');
      const sendClass = ls.is_sending ? 'peer-status status-connected' : 'peer-status';
      const sendLabel = ls.is_sending ? 'sending' : 'idle';
      return `<div class="peer-item peer-item--local">
        <span class="peer-slot">Send</span><span class="peer-name editable" data-stream-index="${ls.stream_index}">${label}</span>
        <span class="${sendClass}">${sendLabel}</span>
        <span class="peer-rtt"></span>
      </div>`;
    }).join('');
    const remoteHtml = slots.map(sl => {
      const streamLabel = sl.stream_name ? ` \u2014 ${escapeHtml(sl.stream_name)}` : '';
      const name = sl.display_name
        ? `${escapeHtml(sl.display_name)}${streamLabel} (${escapeHtml(sl.short_id)})`
        : escapeHtml(sl.short_id);
      const rtt = sl.rtt_ms != null ? `${sl.rtt_ms.toFixed(0)}ms` : '...';
      const status = sl.status || 'connecting';
      const statusClass = `peer-status status-${status}`;
      return `<div class="peer-item">
        <span class="peer-slot">Slot ${sl.slot}</span><span class="peer-name">${name}</span>
        <span class="${statusClass}">${escapeHtml(status)}</span>
        <span class="peer-rtt">${rtt}</span>
      </div>`;
    }).join('');
    if (isEditingStreamName) {
      // Only update remote slots, preserve local sends (user is editing)
      const remoteContainer = slotList.querySelector('.remote-slots');
      if (remoteContainer) remoteContainer.innerHTML = remoteHtml;
    } else {
      slotList.innerHTML = localHtml + `<span class="remote-slots">${remoteHtml}</span>`;
      // Attach inline edit handlers to local send names
      slotList.querySelectorAll('.peer-name.editable').forEach(span => {
        span.addEventListener('click', startStreamNameEdit);
      });
    }
  }
}

function renderNetwork(peers) {
  const tbody = document.getElementById('network-table-body');
  if (peers.length === 0) {
    tbody.innerHTML = '<tr><td colspan="8" class="empty">No peers connected</td></tr>';
    return;
  }
  const oldest = networkSnapshots.length > 1 ? networkSnapshots[0] : null;
  tbody.innerHTML = peers.map(p => {
    const name = p.display_name
      ? escapeHtml(p.display_name)
      : escapeHtml(p.peer_id.slice(0, 8));
    const slot = p.slot != null ? `Slot ${p.slot}` : '-';
    const rtt = p.rtt_ms != null ? `${p.rtt_ms.toFixed(0)}ms` : '-';

    let audioRecv = p.audio_recv;
    let sentRemote = p.intervals_sent_remote;
    let fe = p.frames_expected;
    let fr = p.frames_received;

    if (statsMode === 'recent' && oldest) {
      const old = oldest.get(p.peer_id);
      if (old) {
        audioRecv = Math.max(0, p.audio_recv - old.audio_recv);
        sentRemote = Math.max(0, p.intervals_sent_remote - old.intervals_sent_remote);
        fe = Math.max(0, p.frames_expected - old.frames_expected);
        fr = Math.max(0, p.frames_received - old.frames_received);
      }
    }

    let health = '-';
    let healthClass = '';
    if (fe > 0) {
      const pct = fr / fe * 100;
      health = `${fr}/${fe} (${pct.toFixed(1)}%)`;
      healthClass = pct >= 98 ? 'health-good' : pct >= 90 ? 'health-warn' : 'health-bad';
    }
    return `<tr>
      <td>${name}</td>
      <td>${slot}</td>
      <td class="net-state net-${escapeHtml(p.ice_state)}">${escapeHtml(p.ice_state)}</td>
      <td class="net-state net-${escapeHtml(p.dc_sync_state)}">${escapeHtml(p.dc_sync_state)}</td>
      <td class="net-state net-${escapeHtml(p.dc_audio_state)}">${escapeHtml(p.dc_audio_state)}</td>
      <td>${rtt}</td>
      <td>${fr}</td>
      <td class="${healthClass}">${health}</td>
    </tr>`;
  }).join('');
}

// --- Event Listeners ---
async function setupListeners() {
  cleanup();

  unlisten.push(await listen('status:update', (event) => {
    const s = event.payload;
    lastStatusPayload = s;
    statusSnapshots.push({
      audio_sent: s.audio_sent, audio_recv: s.audio_recv,
      audio_bytes_sent: s.audio_bytes_sent, audio_bytes_recv: s.audio_bytes_recv,
    });
    if (statusSnapshots.length > STATS_WINDOW_SIZE) statusSnapshots.shift();
    // Compute expected frames per interval for debug viz pre-sizing
    if (s.bpm > 0 && s.interval_bars > 0) {
      const quantum = 4.0; // TODO: expose quantum in StatusUpdate if needed
      const beatsPerInterval = s.interval_bars * quantum;
      const intervalSec = beatsPerInterval / (s.bpm / 60.0);
      debugExpectedFrames = Math.round(intervalSec / 0.020); // 20ms per frame
    }
    renderStatus(s);
  }));

  unlisten.push(await listen('tempo:changed', (event) => {
    sessionBpmInput.value = event.payload.bpm.toFixed(1);
  }));

  unlisten.push(await listen('session:error', (event) => {
    showError(sessionError, event.payload.message);
  }));

  unlisten.push(await listen('session:ended', () => {
    showJoin();
  }));

  unlisten.push(await listen('plugin:connected', () => {
    document.getElementById('session-plugin').textContent = 'connected';
    document.getElementById('session-plugin').className = 'status-value connected';
  }));

  unlisten.push(await listen('plugin:disconnected', () => {
    document.getElementById('session-plugin').textContent = 'disconnected';
    document.getElementById('session-plugin').className = 'status-value';
  }));

  unlisten.push(await listen('log:entry', (event) => {
    const p = event.payload;
    addLogEntry(p.level, p.message, p.peer_name || p.peer_id || null);
  }));

  unlisten.push(await listen('chat:message', (event) => {
    const p = event.payload;
    addChatMessage(p.sender_name, p.is_own, p.text);
  }));

  unlisten.push(await listen('peers:network', (event) => {
    const peers = event.payload.peers;
    lastNetworkPeers = peers;
    const snap = new Map();
    for (const p of peers) {
      snap.set(p.peer_id, {
        audio_recv: p.audio_recv, intervals_sent_remote: p.intervals_sent_remote,
        frames_expected: p.frames_expected, frames_received: p.frames_received,
      });
    }
    networkSnapshots.push(snap);
    if (networkSnapshots.length > STATS_WINDOW_SIZE) networkSnapshots.shift();
    renderNetwork(peers);
  }));

  unlisten.push(await listen('debug:interval-frame', debugHandleFrame));

  unlisten.push(await listen('debug:link-tick', (event) => {
    const t = event.payload;
    debugLinkBeat = t.beat;
    const linkInfo = document.getElementById('debug-link-info');
    if (linkInfo && lastStatusPayload) {
      const s = lastStatusPayload;
      const beat = t.beat.toFixed(2);
      const phase = t.phase.toFixed(3);
      const bar = Math.floor(t.beat / 4) + 1;
      const beatInBar = (t.beat % 4).toFixed(2);
      linkInfo.innerHTML =
        `<b>${t.bpm.toFixed(1)}</b> BPM &nbsp;·&nbsp; ` +
        `beat <b>${beat}</b> &nbsp;·&nbsp; ` +
        `bar ${bar}:${beatInBar} &nbsp;·&nbsp; ` +
        `phase ${phase} &nbsp;·&nbsp; ` +
        `Link peers: ${s.link_peers} &nbsp;·&nbsp; ` +
        `interval: ${s.interval_bars} bars`;
    }
    // Re-render canvas to update playhead position
    requestAnimationFrame(debugRender);
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

// --- Chat panel ---
const MAX_CHAT_ENTRIES = 200;

function sendChatMessage() {
  const text = chatInput.value.trim();
  if (!text) return;
  chatInput.value = '';
  invoke('send_chat', { text }).catch(err => console.error('Send chat error:', err));
}

function addChatMessage(senderName, isOwn, text) {
  const time = new Date().toLocaleTimeString();
  const entry = document.createElement('div');
  entry.className = 'chat-entry' + (isOwn ? ' chat-own' : '');

  const sender = document.createElement('span');
  sender.className = 'chat-sender';
  sender.textContent = isOwn ? 'You' : senderName;

  const timeSpan = document.createElement('span');
  timeSpan.className = 'chat-time';
  timeSpan.textContent = time;

  const messageText = document.createElement('span');
  messageText.className = 'chat-text';
  messageText.textContent = text;

  entry.appendChild(sender);
  entry.appendChild(timeSpan);
  entry.appendChild(messageText);

  chatMessages.appendChild(entry);

  // Cap at MAX_CHAT_ENTRIES
  while (chatMessages.children.length > MAX_CHAT_ENTRIES) {
    chatMessages.removeChild(chatMessages.firstChild);
  }

  // Auto-scroll to bottom
  chatMessages.scrollTop = chatMessages.scrollHeight;
}

function clearChatMessages() {
  chatMessages.innerHTML = '';
  chatInput.value = '';
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

function startStreamNameEdit(e) {
  const span = e.currentTarget;
  const streamIndex = parseInt(span.dataset.streamIndex, 10);
  const currentName = span.textContent;
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'stream-name-input';
  input.value = currentName.startsWith('My Send') ? '' : currentName;
  input.maxLength = 32;
  input.placeholder = 'Name this send...';

  let committed = false;
  let cancelled = false;
  const commit = () => {
    if (committed || cancelled) return;
    committed = true;
    const name = input.value.trim();
    invoke('rename_stream', { streamIndex, name });
    // The next status:update will re-render with the new name
  };

  input.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      commit();
      input.blur();
    } else if (ev.key === 'Escape') {
      ev.preventDefault();
      cancelled = true;
      input.replaceWith(span);
    }
  });
  input.addEventListener('blur', () => {
    if (!cancelled) commit();
  });

  span.replaceWith(input);
  input.focus();
  input.select();
}

// --- Debug interval visualization ---
const DEBUG_COL_WIDTH = 200;
const DEBUG_HEADER_HEIGHT = 28;
const DEBUG_CELL_LABEL_HEIGHT = 14;
const DEBUG_CELL_PAD = 4;
const DEBUG_PIX = 1;
const DEBUG_PIX_GAP = 1;
const DEBUG_CELL_GAP = 4;
const DEBUG_VISIBLE_INTERVALS = 2; // current (PLAY+REC) and previous

// State per peer
const debugPeers = new Map();
let debugPeerOrder = [];
let debugCurrentInterval = new Map(); // peer_id -> highest interval index
let debugExpectedFrames = 0; // expected frames per interval (from BPM math)
let debugLinkBeat = 0; // current Link beat position (updated at 20ms)

function debugHandleFrame(ev) {
  const f = ev.payload;
  const pid = f.peer_id;
  if (!debugPeers.has(pid)) {
    debugPeers.set(pid, { name: f.display_name || pid.slice(0, 6), isLocal: f.is_local, intervals: new Map() });
    debugPeerOrder = Array.from(debugPeers.keys());
  }
  const peer = debugPeers.get(pid);
  if (f.display_name) peer.name = f.display_name;

  if (!peer.intervals.has(f.interval_index)) {
    peer.intervals.set(f.interval_index, { frames: new Set(), total: null, offsetMs: f.arrival_offset_ms });
  }
  const iv = peer.intervals.get(f.interval_index);
  iv.frames.add(f.frame_number);
  if (f.is_final && f.total_frames != null) {
    iv.total = f.total_frames;
  }
  if (iv.frames.size === 1) {
    iv.offsetMs = f.arrival_offset_ms;
  }

  // Track current interval (highest seen for this peer)
  const prev = debugCurrentInterval.get(pid) || 0;
  if (f.interval_index > prev) {
    debugCurrentInterval.set(pid, f.interval_index);
  }

  // Prune: keep only the 3 visible intervals per peer
  const cur = debugCurrentInterval.get(pid) || 0;
  const oldest = cur - 2;
  for (const idx of peer.intervals.keys()) {
    if (idx < oldest) peer.intervals.delete(idx);
  }

  requestAnimationFrame(debugRender);
}

function debugRender() {
  if (!debugCanvas || debugPeerOrder.length === 0) return;
  const ctx = debugCanvas.getContext('2d');
  const dpr = window.devicePixelRatio || 1;
  const rect = debugCanvas.getBoundingClientRect();
  const font = getComputedStyle(document.body).fontFamily;

  const numPeers = debugPeerOrder.length;
  const labelW = 40; // left label area for interval number
  const colW = Math.min(DEBUG_COL_WIDTH, (rect.width - labelW - 10) / numPeers);
  const startX = labelW + 6;
  const step = DEBUG_PIX + DEBUG_PIX_GAP;
  const innerW = colW - DEBUG_CELL_PAD * 2 - 4;
  const pixCols = Math.max(1, Math.floor(innerW / step));

  // Global current interval (highest across all peers)
  let globalCurrent = 0;
  for (const [, cur] of debugCurrentInterval) {
    if (cur > globalCurrent) globalCurrent = cur;
  }

  // 2 visible intervals: current (PLAY+REC) and previous
  const visibleIndices = [globalCurrent, globalCurrent - 1];
  const roles = ['PLAY', 'prev'];

  // Compute cell height — pre-sized from BPM math so cells don't grow
  function cellHeightForInterval() {
    const total = debugExpectedFrames || 400;
    const pixRows = Math.ceil(total / pixCols);
    return DEBUG_CELL_LABEL_HEIGHT + pixRows * step + DEBUG_CELL_PAD;
  }

  // Pre-compute row positions
  const rowYs = [];
  const rowHs = [];
  let curY = DEBUG_HEADER_HEIGHT;
  const cellH = cellHeightForInterval();
  for (let i = 0; i < DEBUG_VISIBLE_INTERVALS; i++) {
    const h = cellH;
    rowYs.push(curY);
    rowHs.push(h);
    curY += h + DEBUG_CELL_GAP;
  }

  const totalH = curY + 10;
  debugCanvas.width = rect.width * dpr;
  debugCanvas.height = Math.max(rect.height, totalH) * dpr;
  ctx.scale(dpr, dpr);
  ctx.clearRect(0, 0, rect.width, Math.max(rect.height, totalH));

  // Column headers
  ctx.font = '11px ' + font;
  ctx.textAlign = 'center';
  ctx.fillStyle = '#8b8b9e';
  debugPeerOrder.forEach((pid, colIdx) => {
    const peer = debugPeers.get(pid);
    const x = startX + colIdx * colW + colW / 2;
    const label = peer.isLocal ? `${peer.name} (local)` : peer.name;
    ctx.fillText(label, x, 16);
  });

  // Draw rows
  for (let i = 0; i < DEBUG_VISIBLE_INTERVALS; i++) {
    const idx = visibleIndices[i];
    const role = roles[i];
    const cellY = rowYs[i];
    const cellH = rowHs[i];
    const isActive = role === 'PLAY';

    // Large interval number label on the left
    ctx.font = 'bold 18px ' + font;
    ctx.textAlign = 'right';
    ctx.fillStyle = isActive ? '#4e9af1' : '#55556a';
    ctx.fillText(String(idx), labelW, cellY + cellH / 2 + 6);
    // Role label below number
    ctx.font = '9px ' + font;
    ctx.fillStyle = isActive ? '#4e9af180' : '#55556a60';
    ctx.fillText(role, labelW, cellY + cellH / 2 + 18);

    // Draw cell for each peer
    debugPeerOrder.forEach((pid, colIdx) => {
      const peer = debugPeers.get(pid);
      const iv = peer.intervals.get(idx);
      const cellX = startX + colIdx * colW + 2;
      const cw = colW - 4;

      // Cell container
      ctx.fillStyle = isActive ? '#1e1e28' : '#1c1c21';
      ctx.fillRect(cellX, cellY, cw, cellH);
      ctx.strokeStyle = isActive ? '#4e9af1' : '#2a2a33';
      ctx.lineWidth = isActive ? 1.5 : 1;
      ctx.strokeRect(cellX + 0.5, cellY + 0.5, cw - 1, cellH - 1);

      if (!iv) return;

      const total = iv.total || debugExpectedFrames || Math.max(iv.frames.size, 1);

      // Stats label inside cell (top-right)
      ctx.font = '9px ' + font;
      ctx.textAlign = 'right';
      const pct = iv.total ? Math.round((iv.frames.size / iv.total) * 100) : null;
      ctx.fillStyle = isActive ? '#8b8b9e' : pct === null ? '#8b8b9e' : pct >= 95 ? '#34d399' : pct >= 80 ? '#fbbf24' : '#f87171';
      ctx.fillText(`${iv.frames.size}/${total}`, cellX + cw - 4, cellY + 11);

      // Frame pixel grid
      const frameY = cellY + DEBUG_CELL_LABEL_HEIGHT;
      const frameX = cellX + DEBUG_CELL_PAD;

      // Compute playhead from Link beat position.
      // Local peer: playhead in PLAY row (current interval).
      // Remote peers: playhead in prev row (NINJAM 1-interval latency).
      let playheadFrame = -1;
      const showPlayhead = peer.isLocal ? isActive : !isActive;
      if (showPlayhead && lastStatusPayload) {
        const beatsPerInterval = lastStatusPayload.interval_bars * 4.0;
        // For remote peers, the playhead interval is current-1 (prev row),
        // but the beat position is still in the current interval.
        // Map beat position into the prev interval by using the current beat fraction.
        const playheadIdx = peer.isLocal ? idx : idx + 1;
        const intervalStartBeat = playheadIdx * beatsPerInterval;
        const beatInInterval = debugLinkBeat - intervalStartBeat;
        if (beatInInterval >= 0 && beatInInterval < beatsPerInterval) {
          const frac = beatInInterval / beatsPerInterval;
          playheadFrame = Math.floor(frac * total);
        }
      }

      for (let fn = 0; fn < total; fn++) {
        const col = fn % pixCols;
        const row = Math.floor(fn / pixCols);
        const px = frameX + col * step;
        const py = frameY + row * step;

        if (fn === playheadFrame) {
          ctx.fillStyle = '#ffffff'; // bright white playhead
        } else if (iv.frames.has(fn)) {
          ctx.fillStyle = '#34d399'; // green — received
        } else if (!isActive && iv.total !== null) {
          ctx.fillStyle = '#f87171'; // red — dropped (only in completed intervals)
        } else {
          ctx.fillStyle = '#2a2a30'; // dark — not yet received
        }
        ctx.fillRect(px, py, DEBUG_PIX, DEBUG_PIX);
      }
    });
  }
}

let debugTabActive = false;
let debugRenderTimer = null;

function debugSetActive(active) {
  debugTabActive = active;
  if (active && !debugRenderTimer) {
    debugRenderTimer = setInterval(() => {
      if (debugTabActive) debugRender();
    }, 500);
  } else if (!active && debugRenderTimer) {
    clearInterval(debugRenderTimer);
    debugRenderTimer = null;
  }
}

function debugReset() {
  debugPeers.clear();
  debugPeerOrder = [];
  debugCurrentInterval.clear();
  debugLinkBeat = 0;
  debugSetActive(false);
  if (debugCanvas) {
    const ctx = debugCanvas.getContext('2d');
    ctx.clearRect(0, 0, debugCanvas.width, debugCanvas.height);
  }
}

// Check if a session was auto-started (e.g. via --test-room CLI flag)
invoke('get_active_session').then(result => {
  if (result) {
    showSession(result.room);
    setupListeners();
  }
}).catch(() => {});
