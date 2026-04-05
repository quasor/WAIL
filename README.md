# WAIL — WAN Audio Interchange for Link

WAIL synchronizes [Ableton Link](https://www.ableton.com/link/) sessions across the internet using a WebSocket relay server. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN, with intervalic audio (NINJAM-style) captured, Opus-encoded, and transmitted via the server.

## Install

Download the latest release from the [Releases page](https://github.com/MostDistant/WAIL/releases).

**macOS** — Open the DMG and drag WAIL to Applications. The WAIL Send and Recv plugins are automatically installed to your plugin directories on first launch.

**macOS (Homebrew, from source)** — Build and install directly from source:

```sh
brew tap MostDistant/wail
brew install MostDistant/wail/wail
```

This builds the WAIL binary and DAW plugins from source. After installation, run the plugin installer to copy the CLAP and VST3 bundles into your DAW's plugin directories:

```sh
wail-install-plugins
```

Then rescan plugins in your DAW.

**Windows** — Install via Chocolatey: `choco install wail --source <release-url>`. Use `--params "'/VST3Dir:path /CLAPDir:path'"` for custom plugin directories (defaults to `%CommonProgramFiles%\VST3` and `%CommonProgramFiles%\CLAP`). Uninstalling via `choco uninstall wail` also removes the plugins.

**Linux** — Install the `.deb` package (`sudo dpkg -i wail_*.deb`) or download the AppImage and make it executable (`chmod +x WAIL_*.AppImage`). The WAIL Send and Recv plugins are automatically installed to `~/.clap/` and `~/.vst3/` on first launch.

## Getting Started

1. **Launch the WAIL app.**

2. **Enable Ableton Link in your DAW.** WAIL relies on Link for tempo and phase sync.
   - *Ableton Live:* Preferences > Link, Tempo, MIDI > turn on "Show Link Toggle", then enable Link in the transport bar.
   - *Bitwig Studio:* Settings > Synchronization > enable Link.
   - *REAPER:* Install [ReaBlink](https://github.com/ak5k/reablink), which adds Ableton Link support via a REAPER extension.
   - Other DAWs — check your DAW's documentation for Link support.

3. **Load WAIL Send** on each track or bus you want to share. Each instance captures audio and sends it to your peers at each interval boundary. Use the **Stream Index** parameter (0–14) to assign each instance a unique stream — e.g., drums on stream 0, synth on stream 1.

4. **Load WAIL Recv** on a separate track to hear remote peers. It decodes incoming audio and provides a main mix output plus up to 15 per-slot auxiliary outputs (one per unique peer/stream combination).

5. **Join a room** in the WAIL app. On first launch, you'll be prompted to enter a display name (you can change it later via the settings gear icon). Enter a room name and optionally set a password to create a private room, or leave it blank for a public room. You can also browse existing public rooms from the "Public Rooms" tab.

6. **Play.** Audio is recorded for the duration of each interval (default: 4 bars), then transmitted to all connected peers. Playback runs one interval behind — this latency-by-design is how NINJAM-style sync works.

## Headless CLI Mode

WAIL can run without the GUI for scripted or automated use. The `-headless` flag starts the app in CLI mode, and `-wav` streams a WAV file to peers in the room, looping continuously until stopped.

```sh
./wail-app -headless -room=myroom -wav=song.wav -bpm=120 -name="wav-bot"
```

| Flag | Description |
|------|-------------|
| `-headless` | Run without GUI (required for CLI mode) |
| `-room` | Room to join (required in headless mode) |
| `-wav` | WAV file to send (loaded into memory, resampled to 48kHz stereo) |
| `-bpm` | Tempo in BPM (default: 120) |
| `-name` | Display name (auto-generated if empty) |
| `-password` | Room password (optional) |

Stop with Ctrl+C or SIGTERM for clean shutdown.

## Components

WAIL has three components that work together:

- **WAIL app** — The desktop app that handles networking. It connects to the signaling server, which relays sync and audio data between the DAW plugins and remote peers.

- **WAIL Send** (CLAP/VST3 plugin) — Place this on a track or bus in your DAW to capture audio. At each interval boundary, the recorded audio is Opus-encoded and sent to all connected peers via the WAIL app. You can load multiple instances with different Stream Index values to send separate audio streams (e.g., drums and synth independently).

- **WAIL Recv** (CLAP/VST3 plugin) — Place this on a track in your DAW to hear remote peers. It receives and decodes incoming audio intervals, mixing them into the main output with up to 15 auxiliary outputs (one per unique peer/stream slot).

## Settings

- **Display name** — Shown to other peers in the session.
- **Save debug log locally** — Writes structured logs to a rotating file in the app data directory. Useful for diagnosing connection issues.
- **Peer log streaming** — When enabled, your app's INFO-level logs are broadcast to all other peers in the session via the signaling server, and their logs are shown in your session log panel with a peer name prefix. Useful for collaborative debugging. Both sending and receiving are controlled by this single toggle.
- **Remember settings** — Persists room name, password, and display name in localStorage.

## Troubleshooting

**No sync / peers not connecting** — Make sure Ableton Link is enabled in your DAW. WAIL relies on Link for tempo and phase sync.

**No audio from remote peers** — Verify that both WAIL Send and WAIL Recv plugins are loaded and the WAIL app is running and connected to the same room.

**Changing tempo mid-jam** — Not recommended. WAIL uses NINJAM-style intervals, so audio is recorded and played back in full interval chunks. If you change the tempo, the current interval must finish before the new tempo takes effect. If you do need to change tempo, agree on it beforehand and have one person change it — Link will propagate it to all peers within a few seconds.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for build instructions, project structure, and testing.

## Thanks

WAIL's intervalic audio model is directly inspired by [NINJAM](https://www.ninjam.com/), created by Justin Frankel at [Cockos](https://www.cockos.com/). The idea that you can jam with anyone in the world by accepting one interval of latency changed everything.

Built on the shoulders of great open-source projects:
[Ableton Link](https://www.ableton.com/link/) (tempo/phase sync),
[nih-plug](https://github.com/robbert-vdh/nih-plug) (CLAP/VST3 plugin framework),
[Opus](https://opus-codec.org/) (audio codec),
[Wails](https://wails.io/) (desktop app framework).

Thanks to early supporters [Jeff Hopkins](https://www.youtube.com/@JeffHopkinsMusic) and [Geren M](https://www.youtube.com/@GerenM63) for testing, feedback, and encouragement.

## License

MIT
