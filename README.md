# WAIL — WebRTC Audio Interchange for Link

WAIL synchronizes [Ableton Link](https://www.ableton.com/link/) sessions across the internet using WebRTC. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN, with intervalic audio (NINJAM-style) captured, Opus-encoded, and transmitted peer-to-peer.

## Install

Download the latest release from the [Releases page](https://github.com/quasor/WAIL/releases).

**macOS** — Open the DMG and drag WAIL to Applications. Run the included `.pkg` installer to install the audio plugins.

**macOS (Homebrew, from source)** — Build and install directly from source:

```sh
brew tap quasor/wail
brew install quasor/wail/wail
```

This builds the WAIL binary and DAW plugins from source. The CLAP and VST3 plugins are automatically installed to `~/Library/Audio/Plug-Ins/` — just rescan plugins in your DAW. Note: the Homebrew install provides the `wail` command-line binary. For the full macOS `.app` bundle (dock icon, menu bar), use the DMG installer above.

**Windows** — Run the `.exe` installer. Copy the bundled `.clap` and `.vst3` plugin files to your DAW's plugin directory.

**Linux** — Install the `.deb` package (`sudo dpkg -i wail_*.deb`) or download the AppImage and make it executable (`chmod +x WAIL_*.AppImage`). Copy the plugin files to `~/.clap/` and `~/.vst3/`.

## Getting Started

1. **Launch the WAIL app.**

2. **Enable Ableton Link in your DAW.** WAIL relies on Link for tempo and phase sync.
   - *Ableton Live:* Preferences > Link, Tempo, MIDI > turn on "Show Link Toggle", then enable Link in the transport bar.
   - *Bitwig Studio:* Settings > Synchronization > enable Link.
   - *REAPER:* Install [ReaBlink](https://github.com/ak5k/reablink), which adds Ableton Link support via a REAPER extension.
   - Other DAWs — check your DAW's documentation for Link support.

3. **Load WAIL Send** on the track or bus you want to share. This plugin captures audio and sends it to your peers at each interval boundary.

4. **Load WAIL Recv** on a separate track to hear remote peers. It decodes incoming audio and provides a main mix output plus per-peer auxiliary outputs.

5. **Join a room** in the WAIL app. Enter a room name and your display name. Set a password to create a private room, or leave it blank for a public room. You can also browse existing public rooms from the "Public Rooms" tab.

6. **Play.** Audio is recorded for the duration of each interval (default: 4 bars), then transmitted to all connected peers. Playback runs one interval behind — this latency-by-design is how NINJAM-style sync works.

## Components

WAIL has three components that work together:

- **WAIL app** — The desktop app that handles networking. It connects to the signaling server, establishes WebRTC peer connections, and bridges audio and sync data between the DAW plugins and remote peers.

- **WAIL Send** (CLAP/VST3 plugin) — Place this on a track or bus in your DAW to capture audio. At each interval boundary, the recorded audio is Opus-encoded and sent to all connected peers via the WAIL app.

- **WAIL Recv** (CLAP/VST3 plugin) — Place this on a track in your DAW to hear remote peers. It receives and decodes incoming audio intervals, mixing them into the main output with additional per-peer auxiliary outputs.

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
[webrtc-rs](https://github.com/webrtc-rs/webrtc) (pure Rust WebRTC),
[nih-plug](https://github.com/robbert-vdh/nih-plug) (CLAP/VST3 plugin framework),
[Opus](https://opus-codec.org/) (audio codec),
[Tauri](https://tauri.app/) (desktop app framework).

Thanks to early supporters [Jeff Hopkins](https://www.youtube.com/@JeffHopkinsMusic) and [Geren M](https://www.youtube.com/@GerenM63) for testing, feedback, and encouragement.

## License

MIT
