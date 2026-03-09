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
  document.getElementById('session-audio').textContent = '0 / 0';
  document.getElementById('session-audio-bytes').textContent = '0 B / 0 B';
  document.getElementById('session-plugin').textContent = 'disconnected';
  document.getElementById('session-plugin').className = 'status-value';
  document.getElementById('session-link-peers').textContent = '0';
  document.getElementById('session-interval').textContent = '-';
  testToneEnabled = document.getElementById('test-tone').checked;
  updateTestToneUI();
  document.getElementById('recording-stat').style.display =
    document.getElementById('recording-enabled').checked ? '' : 'none';
}

function updateTestToneUI() {
  document.getElementById('session-test-tone').textContent = testToneEnabled ? 'ON' : 'OFF';
  document.getElementById('session-test-tone').className = testToneEnabled ? 'status-value connected' : 'status-value';
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
    const bpmInput = sessionBpmInput;
    if (document.activeElement !== bpmInput) {
      bpmInput.value = s.bpm.toFixed(1);
    }
    document.getElementById('session-link-peers').textContent = s.link_peers;
    document.getElementById('link-no-peers-warning').style.display =
      (s.link_peers === 0 && s.plugin_connected) ? '' : 'none';
    document.getElementById('session-audio').textContent =
      `${s.audio_sent} / ${s.audio_recv}`;
    document.getElementById('session-audio-bytes').textContent =
      `${formatBytes(s.audio_bytes_sent)} / ${formatBytes(s.audio_bytes_recv)}`;
    document.getElementById('session-interval').textContent = `${s.interval_bars} bar${s.interval_bars !== 1 ? 's' : ''}`;
    document.getElementById('session-plugin').textContent =
      s.plugin_connected ? 'connected' : 'disconnected';
    document.getElementById('session-plugin').className =
      s.plugin_connected ? 'status-value connected' : 'status-value';

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
          <span class="peer-slot">Slot ${sl.slot}</span><span class="peer-name">${name}</span>
          <span class="${statusClass}">${escapeHtml(status)}</span>
          <span class="peer-rtt">${rtt}</span>
        </div>`;
      }).join('');
    }
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

// Check if a session was auto-started (e.g. via --test-room CLI flag)
invoke('get_active_session').then(result => {
  if (result) {
    showSession(result.room);
    setupListeners();
  }
}).catch(() => {});
