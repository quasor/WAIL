package main

import (
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"log"
	"math"
	"net"
	"sort"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
)

// SessionConfig holds configuration for a session.
type SessionConfig struct {
	Server      string
	Room        string
	Password    *string
	DisplayName string
	Identity    string
	BPM         float64
	Bars        uint32
	Quantum     float64
	IPCPort     uint16
	Recording   *RecordingConfig
	StreamCount uint16
	TestMode    bool
}

// SessionCommand represents commands from the UI to the session.
type SessionCommand struct {
	Type        string // "ChangeBpm", "SendChat", "StreamNamesChanged", "SetTestTone", "SetWavSender", "Disconnect"
	BPM         float64
	Text        string
	Names       map[uint16]string
	StreamIndex *uint16
	WavFile     string
}

// SessionHandle represents a running session.
type SessionHandle struct {
	CmdCh  chan SessionCommand
	PeerID string
	Room   string
	cancel context.CancelFunc
	done   chan struct{} // closed when session goroutine exits
}

// EventEmitter abstracts frontend event emission.
type EventEmitter interface {
	Emit(event string, data any)
}

// SpawnSession starts a new session in a goroutine.
func SpawnSession(emitter EventEmitter, config SessionConfig) (*SessionHandle, error) {
	cmdCh := make(chan SessionCommand, 64)
	peerID := generateShortID()

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})

	handle := &SessionHandle{
		CmdCh:  cmdCh,
		PeerID: peerID,
		Room:   config.Room,
		cancel: cancel,
		done:   done,
	}

	go func() {
		defer close(done)
		if err := sessionLoop(ctx, emitter, config, peerID, cmdCh); err != nil {
			log.Printf("[session] Error: %v", err)
			emitter.Emit("session:error", SessionError{Message: err.Error()})
		}
		emitter.Emit("session:ended", SessionEnded{})
	}()

	return handle, nil
}

func generateShortID() string {
	return uuid.New().String()[:8]
}

func computeIntervalIndex(beat float64, bars uint32, quantum float64) int64 {
	beatsPerInterval := float64(bars) * quantum
	return int64(math.Floor(beat / beatsPerInterval))
}

func beatsPerInterval(bars uint32, quantum float64) float64 {
	return float64(bars) * quantum
}

func sessionLoop(
	ctx context.Context,
	emitter EventEmitter,
	config SessionConfig,
	peerID string,
	cmdCh <-chan SessionCommand,
) error {
	displayName := config.DisplayName
	identity := config.Identity
	room := config.Room
	bpm := config.BPM
	bars := config.Bars
	quantum := config.Quantum

	logInfo := func(msg string, args ...any) {
		formatted := fmt.Sprintf(msg, args...)
		log.Printf("[session] %s", formatted)
		emitter.Emit("log:entry", LogEntry{Level: "info", Message: formatted})
	}
	logWarn := func(msg string, args ...any) {
		formatted := fmt.Sprintf(msg, args...)
		log.Printf("[session] WARN: %s", formatted)
		emitter.Emit("log:entry", LogEntry{Level: "warn", Message: formatted})
	}

	logInfo("Starting peer %s as %s in room %s (BPM %.0f, %d bars, quantum %.0f)", peerID, displayName, room, bpm, bars, quantum)

	// Initialize Ableton Link
	link := NewLinkBridge(bpm, quantum)
	link.Enable()
	linkCmdCh, linkEventCh := link.SpawnPoller(ctx)
	logInfo("Ableton Link enabled")

	// Connect to signaling server
	mesh, syncRx, audioRx, err := connectMesh(ctx, config, peerID)
	if err != nil {
		return fmt.Errorf("signaling connect: %w", err)
	}
	logInfo("Connected to signaling server at %s", config.Server)

	emitter.Emit("session:started", SessionStarted{PeerID: peerID, Room: room, BPM: bpm})

	// State
	clock := NewClockSync()
	peers := NewPeerRegistry()
	if names := mesh.TakeInitialPeerNames(); names != nil {
		peers.SeedNames(names)
	}
	ipcPool := NewIPCWriterPool()

	var lastIntervalIndex *int64
	intervalBars := bars
	intervalQuantum := quantum
	lastBroadcastBPM := bpm
	var initialBeatSynced, isJoiner bool
	localStreamNames := make(map[uint16]string)

	// Audio stats
	var audioIntervalsSent, audioIntervalsReceived uint64
	var audioBytesSent, audioBytesRecv uint64
	var audioStatusSeq uint64
	var intervalFramesSent, intervalFramesRecv uint64
	var intervalBytesSent, intervalBytesRecv uint64
	var ipcDropCount atomic.Uint64
	var boundaryDriftUs *int64

	// Local send tracking
	localSendStreams := make(map[int]uint16) // connID → streamIndex
	localSendActive := make(map[uint16]bool)
	loggedFirstFrameSent := false

	// Debug frame tracking (per peer:stream, reset per Link interval)
	debugFrameCounters := make(map[string]uint32)
	var debugLastInterval int64 = -1

	// Test tone state
	var testToneBoundaryCh chan IntervalBoundaryInfo
	var testToneCancelFn context.CancelFunc
	var testToneStream *uint16

	// WAV sender state
	var wavSenderBoundaryCh chan IntervalBoundaryInfo
	var wavSenderCancelFn context.CancelFunc
	var wavSenderStream *uint16

	// IPC
	ipcFromPluginCh := make(chan ipcFrame, 64)
	ipcDisconnectCh := make(chan int, 16)
	ipcSendRegCh := make(chan ipcSendRegistration, 16) // send stream registrations from IPC goroutine
	var nextConnID int

	// Recording
	var recorder *SessionRecorder
	if config.Recording != nil && config.Recording.Enabled {
		r, err := StartRecording(*config.Recording, room)
		if err != nil {
			logWarn("Failed to start recording: %v", err)
		} else {
			recorder = r
			logInfo("Recording enabled: %s", config.Recording.Directory)
		}
	}

	// Start IPC listener
	bindPort := config.IPCPort
	if config.TestMode {
		bindPort = 0
	}
	listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", bindPort))
	if err != nil {
		return fmt.Errorf("IPC listen: %w", err)
	}
	defer listener.Close()
	logInfo("IPC listening on %s", listener.Addr())

	// Accept IPC connections in goroutine
	go acceptIPCConnections(ctx, listener, ipcFromPluginCh, ipcDisconnectCh, ipcSendRegCh, ipcPool, peers, &nextConnID, &ipcDropCount, emitter, logInfo, logWarn)

	// Timers
	pingTicker := time.NewTicker(time.Duration(PingIntervalMs) * time.Millisecond)
	defer pingTicker.Stop()
	statusTicker := time.NewTicker(2 * time.Second)
	defer statusTicker.Stop()
	livenessTicker := time.NewTicker(5 * time.Second)
	defer livenessTicker.Stop()

	var lastBoundaryTime *time.Time

	// Signaling reconnect state
	type sigReconnect struct {
		attempt uint32
		nextTry time.Time
	}
	var reconnect *sigReconnect
	reconnectTimer := time.NewTimer(time.Hour)
	reconnectTimer.Stop()

	// Signaling event goroutine → channel
	sigEventCh := make(chan *MeshEvent, 64)
	sigClosedCh := make(chan struct{})
	var sigMu sync.Mutex
	currentSyncRx := syncRx
	currentAudioRx := audioRx

	go func() {
		for {
			ev, ok := mesh.PollSignaling()
			if !ok {
				close(sigClosedCh)
				return
			}
			if ev != nil {
				sigEventCh <- ev
			}
		}
	}()

	logInfo("Waiting for peers...")

	for {
		select {
		case <-ctx.Done():
			goto cleanup

		// --- UI commands ---
		case cmd := <-cmdCh:
			switch cmd.Type {
			case "ChangeBpm":
				logInfo("BPM changed to %.1f", cmd.BPM)
				lastBroadcastBPM = cmd.BPM
				linkCmdCh <- LinkCommand{Type: "SetTempo", BPM: cmd.BPM}
			case "SendChat":
				msg := NewChatMessage(displayName, cmd.Text)
				mesh.Broadcast(msg)
				emitter.Emit("chat:message", ChatMessageEvent{SenderName: displayName, IsOwn: true, Text: cmd.Text})
			case "StreamNamesChanged":
				localStreamNames = cmd.Names
				mesh.Broadcast(NewStreamNames(StreamNamesToWire(localStreamNames)))
			case "SetTestTone":
				// Stop existing test tone
				if testToneCancelFn != nil {
					testToneCancelFn()
					testToneCancelFn = nil
				}
				testToneBoundaryCh = nil
				if testToneStream != nil {
					delete(localStreamNames, *testToneStream)
				}
				testToneStream = nil

				if cmd.StreamIndex != nil {
					si := *cmd.StreamIndex
					toneCtx, cancelFn := context.WithCancel(ctx)
					testToneCancelFn = cancelFn
					boundaryCh := make(chan IntervalBoundaryInfo, 4)
					testToneBoundaryCh = boundaryCh
					testToneStream = &si

					connID := int(^uint(0)>>1) - int(si)
					localSendStreams[connID] = si

					toneName := "Test Tone"
					if displayName != "" {
						toneName = displayName + "'s Test Tone"
					}
					localStreamNames[si] = toneName

					go TestToneTask(toneCtx, si, connID, ipcFromPluginCh, boundaryCh)
					logInfo("[TEST] Test tone started on Send %d", si)
				} else {
					logInfo("[TEST] Test tone stopped")
				}
				mesh.Broadcast(NewStreamNames(StreamNamesToWire(localStreamNames)))
			case "SetWavSender":
				// Stop existing WAV sender
				if wavSenderCancelFn != nil {
					wavSenderCancelFn()
					wavSenderCancelFn = nil
				}
				wavSenderBoundaryCh = nil
				if wavSenderStream != nil {
					delete(localStreamNames, *wavSenderStream)
				}
				wavSenderStream = nil

				if cmd.StreamIndex != nil && cmd.WavFile != "" {
					si := *cmd.StreamIndex
					wavCtx, cancelFn := context.WithCancel(ctx)
					wavSenderCancelFn = cancelFn
					boundaryCh := make(chan IntervalBoundaryInfo, 4)
					wavSenderBoundaryCh = boundaryCh
					wavSenderStream = &si

					connID := int(^uint(0)>>1) - 100 - int(si) // offset from test tone connIDs
					localSendStreams[connID] = si

					wavName := "WAV Sender"
					if displayName != "" {
						wavName = displayName + "'s WAV"
					}
					localStreamNames[si] = wavName

					go WavSenderTask(wavCtx, si, connID, ipcFromPluginCh, boundaryCh, cmd.WavFile)
					logInfo("[WAV] WAV sender started on Send %d: %s", si, cmd.WavFile)
				} else {
					logInfo("[WAV] WAV sender stopped")
				}
				mesh.Broadcast(NewStreamNames(StreamNamesToWire(localStreamNames)))
			case "Disconnect":
				logInfo("Disconnecting...")
				goto cleanup
			}

		// --- Signaling events ---
		case ev := <-sigEventCh:
			switch ev.Type {
			case "PeerJoined":
				display := ev.PeerID
				if ev.DisplayName != nil {
					display = *ev.DisplayName
				}
				logInfo("Peer %s joined room", display)
				peers.Add(ev.PeerID, ev.DisplayName)
				emitter.Emit("peer:joined", PeerJoinedEvent{PeerID: ev.PeerID, DisplayName: ev.DisplayName})

				hello := NewHello(peerID, &displayName, &identity)
				mesh.Broadcast(hello)
				mesh.Broadcast(NewIntervalConfig(bars, quantum))
				if lastIntervalIndex != nil {
					mesh.Broadcast(NewIntervalBoundary(*lastIntervalIndex))
				}
				mesh.Broadcast(NewAudioCapabilities([]uint32{48000}, []uint16{1, 2}, true, true))
				if len(localStreamNames) > 0 {
					mesh.Broadcast(NewStreamNames(StreamNamesToWire(localStreamNames)))
				}

			case "PeerLeft":
				var name string
				peers.WithPeer(ev.PeerID, func(p *PeerState) {
					if p.DisplayName != nil {
						name = *p.DisplayName
					}
				})
				if name == "" {
					name = ev.PeerID
				}
				logInfo("Peer %s left", name)
				removePeerFully(peers, ipcPool, ev.PeerID)
				emitter.Emit("peer:left", PeerLeftEvent{PeerID: ev.PeerID})

			case "PeerListReceived":
				peers.SeedLastSeen()
				isJoiner = ev.PeerCount > 0
				logInfo("Joined room with %d peer(s)", ev.PeerCount)
			}

		case <-sigClosedCh:
			if reconnect == nil {
				logWarn("Signaling connection closed — attempting reconnection")
				emitter.Emit("session:reconnecting", nil)
				reconnect = &sigReconnect{attempt: 1, nextTry: time.Now().Add(time.Second)}
				reconnectTimer.Reset(time.Second)
			}

		// --- Signaling reconnection ---
		case <-reconnectTimer.C:
			if reconnect == nil {
				continue
			}
			attempt := reconnect.attempt
			if attempt == 10 {
				logWarn("Signaling reconnection stale after %d attempts", attempt)
				emitter.Emit("session:stale", SessionStale{Attempts: attempt})
			}
			logInfo("Signaling reconnect attempt %d...", attempt)

			newChannels, newNames, err := mesh.ReconnectSignaling(ctx, config.Server, room, config.Password, &displayName)
			if err != nil {
				logWarn("Signaling reconnect failed: %v", err)
				nextAttempt := attempt + 1
				backoffMs := min64(1000*pow2(nextAttempt-1), 30000)
				reconnect.attempt = nextAttempt
				reconnect.nextTry = time.Now().Add(time.Duration(backoffMs) * time.Millisecond)
				reconnectTimer.Reset(time.Duration(backoffMs) * time.Millisecond)
			} else {
				if newNames != nil {
					peers.SeedNames(newNames)
				}
				sigMu.Lock()
				currentSyncRx = newChannels.SyncCh
				currentAudioRx = newChannels.AudioCh
				sigMu.Unlock()

				// Restart signaling poll goroutine
				sigEventCh2 := make(chan *MeshEvent, 64)
				sigClosedCh2 := make(chan struct{})
				sigEventCh = sigEventCh2
				sigClosedCh = sigClosedCh2
				go func() {
					for {
						ev, ok := mesh.PollSignaling()
						if !ok {
							close(sigClosedCh2)
							return
						}
						if ev != nil {
							sigEventCh2 <- ev
						}
					}
				}()

				reconnect = nil
				logInfo("Signaling reconnected (attempt %d)", attempt)
				emitter.Emit("session:reconnected", nil)
			}

		// --- Sync messages from peers ---
		case fps := <-currentSyncRx:
			from := fps.From
			msg := fps.Msg
			peers.WithPeer(from, func(p *PeerState) {
				p.LastSeen = time.Now()
				p.EverReceivedMessage = true
			})

			switch msg.Type {
			case "Hello":
				nameDisplay := "(anonymous)"
				if msg.DisplayName != nil {
					nameDisplay = *msg.DisplayName
				}
				logInfo("Hello from %s (%s)", nameDisplay, msg.PeerID)

				peers.WithPeer(msg.PeerID, func(p *PeerState) {
					p.DisplayName = msg.DisplayName
				})
				if peers.Get(msg.PeerID) == nil {
					peers.Add(msg.PeerID, msg.DisplayName)
				}

				if msg.Identity != nil {
					rid := *msg.Identity
					// Evict stale peer
					if oldPID, found := peers.FindByIdentity(rid); found && oldPID != msg.PeerID {
						logInfo("Peer %s reconnected (old=%s, new=%s) — evicting stale", nameDisplay, oldPID, msg.PeerID)
						removePeerFully(peers, ipcPool, oldPID)
						mesh.RemovePeer(oldPID)
						emitter.Emit("peer:left", PeerLeftEvent{PeerID: oldPID})
					}

					peers.WithPeer(msg.PeerID, func(p *PeerState) {
						p.Identity = msg.Identity
					})
					peers.RekeyPeerSlots(msg.PeerID, rid)
					peers.AssignSlot(msg.PeerID, 0)

					// Notify recv plugins
					if !ipcPool.IsEmpty() {
						ipcPool.Broadcast(EncodeFrame(EncodePeerJoinedMsg(msg.PeerID, rid)))
						if msg.DisplayName != nil {
							ipcPool.Broadcast(EncodeFrame(EncodePeerNameMsg(msg.PeerID, *msg.DisplayName)))
						}
					}
				}

				if peers.MarkHelloSent(from) {
					reply := NewHello(peerID, &displayName, &identity)
					mesh.SendTo(from, reply)
					if len(localStreamNames) > 0 {
						mesh.SendTo(from, NewStreamNames(StreamNamesToWire(localStreamNames)))
					}
				}

				emitter.Emit("peer:joined", PeerJoinedEvent{PeerID: msg.PeerID, DisplayName: msg.DisplayName})

			case "Ping":
				pong := clock.HandlePing(msg.ID, msg.SentAtUs)
				mesh.SendTo(from, pong)

			case "Pong":
				clock.HandlePong(from, msg.PingSentAtUs, msg.PongSentAtUs)

			case "TempoChange":
				var name string
				peers.WithPeer(from, func(p *PeerState) {
					if p.DisplayName != nil {
						name = *p.DisplayName
					}
				})
				if name == "" {
					name = from
				}
				logInfo("Tempo change from %s: %.1f BPM", name, msg.BPM)
				lastBroadcastBPM = msg.BPM
				linkCmdCh <- LinkCommand{Type: "SetTempo", BPM: msg.BPM}
				emitter.Emit("tempo:changed", TempoChangedEvent{BPM: msg.BPM, Source: "remote"})

			case "StateSnapshot":
				if !initialBeatSynced {
					initialBeatSynced = true
					if isJoiner {
						logInfo("Beat sync — snapped to beat %.2f", msg.Beat)
						rttUs := clock.RTTUs(from)
						linkCmdCh <- LinkCommand{Type: "ForceBeat", Beat: msg.Beat, RTTUs: rttUs}
						newIdx := computeIntervalIndex(msg.Beat, intervalBars, intervalQuantum)
						lastIntervalIndex = &newIdx
					} else {
						logInfo("Beat sync — we are room owner, skipping ForceBeat")
					}
				}
				if math.Abs(msg.BPM-lastBroadcastBPM) > 0.01 {
					lastBroadcastBPM = msg.BPM
					linkCmdCh <- LinkCommand{Type: "SetTempo", BPM: msg.BPM}
				}

			case "IntervalConfig":
				logInfo("Remote interval config: %d bars, quantum %.0f", msg.Bars, msg.Quantum)
				intervalBars = msg.Bars
				intervalQuantum = msg.Quantum

			case "AudioStatus":
				peers.WithPeer(from, func(p *PeerState) {
					p.RemoteIntervalsSent = msg.IntervalsSent
				})

			case "ChatMessage":
				emitter.Emit("chat:message", ChatMessageEvent{SenderName: msg.SenderName, IsOwn: false, Text: msg.Text})

			case "StreamNames":
				if msg.Names != nil {
					parsed := StreamNamesFromWire(msg.Names)
					peers.WithPeer(from, func(p *PeerState) {
						p.StreamNames = parsed
					})
				}
			}

		// --- Audio from peers ---
		case fpa := <-currentAudioRx:
			from := fpa.From
			data := fpa.Data
			peers.WithPeer(from, func(p *PeerState) {
				p.LastSeen = time.Now()
				p.EverReceivedMessage = true
				p.AudioRecvCount++
			})

			// Assign slot
			if len(data) >= 7 && data[0] == 'W' && data[1] == 'A' && data[2] == 'I' && data[3] == 'F' {
				streamID := binary.LittleEndian.Uint16(data[5:7])
				peers.AssignSlot(from, streamID)
			}

			// Track frame metrics
			if header := PeekWaifHeader(data); header != nil {
				var dn *string
				var streamName *string
				peers.WithPeer(from, func(p *PeerState) {
					p.TotalFramesReceived++
					var intervalExpected uint64
					if header.IsFinal {
						intervalExpected = uint64(header.TotalFrames)
					} else {
						intervalExpected = uint64(header.FrameNumber) + 1
					}
					prev := p.IntervalFramesExpected[header.IntervalIndex]
					if intervalExpected > prev {
						p.TotalFramesExpected += intervalExpected - prev
						p.IntervalFramesExpected[header.IntervalIndex] = intervalExpected
					}
					if header.IsFinal {
						delete(p.IntervalFramesExpected, header.IntervalIndex)
					}
					if lastIntervalIndex != nil && header.IntervalIndex < *lastIntervalIndex-1 {
						p.LateFrames++
					}
					dn = p.DisplayName
					if n, ok := p.StreamNames[header.StreamID]; ok {
						streamName = &n
					}
				})
				var arrivalOffsetMs float64
				if lastBoundaryTime != nil {
					arrivalOffsetMs = float64(time.Since(*lastBoundaryTime).Milliseconds())
				}
				// Debug: use receiver's Link-aligned interval + own frame counter
				// (sender's raw interval_index/frame_number may not match Link)
				if lastIntervalIndex != nil {
					if *lastIntervalIndex != debugLastInterval {
						for k := range debugFrameCounters {
							delete(debugFrameCounters, k)
						}
						debugLastInterval = *lastIntervalIndex
					}
					counterKey := from + ":" + strconv.Itoa(int(header.StreamID))
					debugFrameNum := debugFrameCounters[counterKey]
					debugFrameCounters[counterKey] = debugFrameNum + 1

					emitter.Emit("debug:interval-frame", DebugIntervalFrame{
						PeerID: from, DisplayName: dn, StreamIndex: header.StreamID,
						StreamName: streamName, IntervalIndex: *lastIntervalIndex,
						FrameNumber: debugFrameNum, TotalFrames: nil,
						IsFinal: false, ArrivalOffsetMs: arrivalOffsetMs,
					})
				}
			}

			audioIntervalsReceived++
			audioBytesRecv += uint64(len(data))
			intervalFramesRecv++
			intervalBytesRecv += uint64(len(data))

			if recorder != nil {
				var name *string
				peers.WithPeer(from, func(p *PeerState) { name = p.DisplayName })
				recorder.RecordPeer(from, name, data)
			}

			// Rewrite interval index
			if lastIntervalIndex != nil {
				RewriteWaifIntervalIndex(data, *lastIntervalIndex)
			}

			// Forward to recv plugins
			if !ipcPool.IsEmpty() {
				ipcPool.Broadcast(EncodeFrame(EncodeAudioMsg(from, data)))
			}

		// --- Audio from plugins ---
		case frame := <-ipcFromPluginCh:
			wireData, ok := DecodeAudioFrameMsg(frame.data)
			if !ok {
				continue
			}
			if lastIntervalIndex == nil {
				continue
			}

			// Track active stream
			if len(wireData) >= 7 && wireData[0] == 'W' && wireData[1] == 'A' {
				streamID := binary.LittleEndian.Uint16(wireData[5:7])
				localSendActive[streamID] = true
			}

			mesh.BroadcastAudio(wireData)
			audioBytesSent += uint64(len(wireData))
			audioIntervalsSent++
			intervalBytesSent += uint64(len(wireData))
			intervalFramesSent++
			if !loggedFirstFrameSent {
				loggedFirstFrameSent = true
				logInfo("audio: first WAIF frame sent (%d bytes, interval=%v)", len(wireData), lastIntervalIndex)
			}

		// --- IPC send stream registration ---
		case reg := <-ipcSendRegCh:
			localSendStreams[reg.ConnID] = reg.StreamIndex

		// --- IPC disconnect ---
		case connID := <-ipcDisconnectCh:
			ipcPool.Remove(connID)
			delete(localSendStreams, connID)

		// --- Link events ---
		case ev := <-linkEventCh:
			switch ev.Type {
			case "TempoChanged":
				if math.Abs(ev.BPM-lastBroadcastBPM) > 0.01 {
					logInfo("Local tempo changed to %.1f BPM", ev.BPM)
					lastBroadcastBPM = ev.BPM
					mesh.Broadcast(NewTempoChange(ev.BPM, quantum, ev.TimestampUs))
					emitter.Emit("tempo:changed", TempoChangedEvent{BPM: ev.BPM, Source: "local"})
				}
				handleIntervalBoundary(ev.Beat, intervalBars, intervalQuantum, lastIntervalIndex, lastBroadcastBPM, lastBoundaryTime, &boundaryDriftUs, mesh, &intervalFramesSent, &intervalFramesRecv, &intervalBytesSent, &intervalBytesRecv, &audioIntervalsSent, &audioIntervalsReceived, &lastIntervalIndex, &lastBoundaryTime, testToneBoundaryCh, wavSenderBoundaryCh)

			case "StateUpdate":
				mesh.Broadcast(NewStateSnapshot(ev.BPM, ev.Beat, ev.Phase, ev.Quantum, ev.TimestampUs))
				handleIntervalBoundary(ev.Beat, intervalBars, intervalQuantum, lastIntervalIndex, lastBroadcastBPM, lastBoundaryTime, &boundaryDriftUs, mesh, &intervalFramesSent, &intervalFramesRecv, &intervalBytesSent, &intervalBytesRecv, &audioIntervalsSent, &audioIntervalsReceived, &lastIntervalIndex, &lastBoundaryTime, testToneBoundaryCh, wavSenderBoundaryCh)
				emitter.Emit("debug:link-tick", LinkTickEvent{BPM: ev.BPM, Beat: ev.Beat, Phase: ev.Phase})
			}

		// --- Ping timer ---
		case <-pingTicker.C:
			ping := clock.MakePing()
			mesh.Broadcast(ping)

		// --- Liveness watchdog ---
		case <-livenessTicker.C:
			for _, deadID := range peers.TimedOutPeers(30 * time.Second) {
				var name string
				peers.WithPeer(deadID, func(p *PeerState) {
					if p.DisplayName != nil {
						name = *p.DisplayName
					}
				})
				if name == "" {
					name = deadID
				}
				logWarn("Peer %s timed out", name)
				removePeerFully(peers, ipcPool, deadID)
				mesh.RemovePeer(deadID)
				emitter.Emit("peer:left", PeerLeftEvent{PeerID: deadID})
			}

			// Hello completion watchdog
			softPeers, hardPeers := peers.NoIdentityActivePeers(5*time.Second, 15*time.Second)
			helloMsg := NewHello(peerID, &displayName, &identity)
			for _, pid := range softPeers {
				logWarn("Peer %s active but Hello not received — re-sending", pid)
				mesh.SendTo(pid, helloMsg)
				peers.MarkHelloRetrySent(pid)
			}
			for _, pid := range hardPeers {
				logWarn("Peer %s no identity after 15s — removing", pid)
				removePeerFully(peers, ipcPool, pid)
				mesh.RemovePeer(pid)
				emitter.Emit("peer:left", PeerLeftEvent{PeerID: pid})
			}

		// --- Status update ---
		case <-statusTicker.C:
			stateCh := make(chan LinkState, 1)
			linkCmdCh <- LinkCommand{Type: "GetState", StateCh: stateCh}
			state, ok := <-stateCh
			if !ok {
				continue
			}

			connected := mesh.ConnectedPeers()
			dcOpen := mesh.AnyPeersConnected()

			// Build peer infos
			peerInfos := make([]PeerInfo, 0, len(connected))
			for _, p := range connected {
				var dn *string
				var recvNow, recvPrev uint64
				peers.WithPeer(p, func(ps *PeerState) {
					dn = ps.DisplayName
					recvNow = ps.AudioRecvCount
					recvPrev = ps.AudioRecvPrev
				})
				isRecv := recvNow > recvPrev
				isSend := dcOpen && mesh.IsPeerConnected(p)
				status := peers.DeriveStatus(p)
				var rttMs *float64
				if rtt := clock.RTTUs(p); rtt != nil {
					v := float64(*rtt) / 1000.0
					rttMs = &v
				}
				var slot *uint32
				if s := peers.SlotFor(p, 0); s >= 0 {
					v := uint32(s + 1)
					slot = &v
				}
				peerInfos = append(peerInfos, PeerInfo{
					PeerID: p, DisplayName: dn, RTTMs: rttMs, Slot: slot,
					Status: status, IsSending: isSend, IsReceiving: isRecv,
				})
			}

			// Build slot infos
			mappings := peers.ActiveMappings()
			slotInfos := make([]SlotInfo, 0, len(mappings))
			for _, m := range mappings {
				var dn *string
				var isSend, isRecv bool
				var streamName *string
				pid, found := peers.FindByIdentity(m.ClientID)
				if found {
					peers.WithPeer(pid, func(ps *PeerState) {
						dn = ps.DisplayName
						isRecv = ps.AudioRecvCount > ps.AudioRecvPrev
						if n, ok := ps.StreamNames[m.ChannelIndex]; ok {
							streamName = &n
						}
					})
					isSend = dcOpen && mesh.IsPeerConnected(pid)
				}
				status := "connecting"
				if found {
					status = peers.DeriveStatus(pid)
				}
				var rttMs *float64
				if found {
					if rtt := clock.RTTUs(pid); rtt != nil {
						v := float64(*rtt) / 1000.0
						rttMs = &v
					}
				}
				slotInfos = append(slotInfos, SlotInfo{
					Slot: uint32(m.SlotIndex + 1), ShortID: m.ShortID(), ClientID: m.ClientID,
					ChannelIndex: m.ChannelIndex, DisplayName: dn, Status: &status,
					RTTMs: rttMs, IsSending: isSend, IsReceiving: isRecv, StreamName: streamName,
				})
			}

			// Build local sends
			localSends := make([]LocalSendInfo, 0, len(localSendStreams))
			for _, streamIdx := range localSendStreams {
				var sn *string
				if n, ok := localStreamNames[streamIdx]; ok {
					sn = &n
				}
				localSends = append(localSends, LocalSendInfo{
					StreamIndex: streamIdx,
					IsSending:   localSendActive[streamIdx],
					StreamName:  sn,
				})
			}
			sort.Slice(localSends, func(i, j int) bool { return localSends[i].StreamIndex < localSends[j].StreamIndex })
			localSendActive = make(map[uint16]bool)

			peers.FlushAudioRecvPrev()

			emitter.Emit("status:update", StatusUpdate{
				BPM: state.BPM, Beat: state.Beat, Phase: state.Phase,
				LinkPeers: state.NumPeers, Peers: peerInfos, Slots: slotInfos,
				LocalSends: localSends, IntervalBars: intervalBars,
				AudioSent: audioIntervalsSent, AudioRecv: audioIntervalsReceived,
				AudioBytesSent: audioBytesSent, AudioBytesRecv: audioBytesRecv,
				AudioDCOpen: dcOpen, PluginConnected: !ipcPool.IsEmpty() || config.TestMode,
				Recording: recorder != nil,
				RecordingSizeBytes: func() uint64 { if recorder != nil { return recorder.BytesWritten() }; return 0 }(),
			})

			// Broadcast audio status
			audioStatusSeq++
			mesh.Broadcast(NewAudioStatus(dcOpen, audioIntervalsSent, audioIntervalsReceived, !ipcPool.IsEmpty() || config.TestMode, audioStatusSeq))

			// Send metrics + build network event
			perPeer := make(map[string]PeerFrameReport)
			networkInfos := make([]PeerNetworkInfo, 0, len(connected))
			for _, p := range connected {
				var fe, fr, lf uint64
				var audioRecv uint64
				peers.WithPeer(p, func(ps *PeerState) {
					fe = ps.TotalFramesExpected
					fr = ps.TotalFramesReceived
					lf = ps.LateFrames
					audioRecv = ps.AudioRecvCount
				})
				perPeer[p] = PeerFrameReport{
					FramesExpected: fe, FramesReceived: fr,
					RTTUs: clock.RTTUs(p), JitterUs: clock.JitterUs(p),
					LateFrames: lf,
				}
				var dn *string
				var rttMs *float64
				var slot *uint32
				for _, pi := range peerInfos {
					if pi.PeerID == p {
						dn = pi.DisplayName
						rttMs = pi.RTTMs
						slot = pi.Slot
						break
					}
				}
				status := peers.DeriveStatus(p)
				var framePct float64
				if fe > 0 {
					framePct = float64(fr) / float64(fe) * 100.0
				}
				networkInfos = append(networkInfos, PeerNetworkInfo{
					PeerID: p, DisplayName: dn, Slot: slot,
					ICEState: status, DCSyncState: status, DCAudioState: status,
					RTTMs: rttMs, AudioRecv: audioRecv,
					FramesExpected: fe, FramesReceived: fr, FramePct: framePct,
				})
			}
			emitter.Emit("peers:network", PeersNetwork{Peers: networkInfos})
			_ = ipcDropCount.Load()
			mesh.SendMetricsReport(dcOpen, !ipcPool.IsEmpty() || config.TestMode, perPeer, ipcDropCount.Load(), boundaryDriftUs)
		}
	}

cleanup:
	if recorder != nil {
		recorder.Finalize()
		logInfo("Recording finalized")
	}
	return nil
}

type ipcFrame struct {
	connID int
	data   []byte
}

type ipcSendRegistration struct {
	ConnID      int
	StreamIndex uint16
}

func connectMesh(ctx context.Context, config SessionConfig, peerID string) (*PeerMesh, <-chan FromPeerSync, <-chan FromPeerAudio, error) {
	client, channels, peerNames, err := ConnectSignaling(
		ctx, config.Server, config.Room, peerID,
		config.Password, config.StreamCount, &config.DisplayName,
	)
	if err != nil {
		return nil, nil, nil, err
	}
	mesh := NewPeerMesh(peerID, client, channels, config.StreamCount, peerNames)
	return mesh, channels.SyncCh, channels.AudioCh, nil
}

func acceptIPCConnections(
	ctx context.Context,
	listener net.Listener,
	fromPluginCh chan<- ipcFrame,
	disconnectCh chan<- int,
	sendRegCh chan<- ipcSendRegistration,
	pool *IPCWriterPool,
	peers *PeerRegistry,
	nextID *int,
	dropCounter *atomic.Uint64,
	emitter EventEmitter,
	logInfo func(string, ...any),
	logWarn func(string, ...any),
) {
	var mu sync.Mutex
	for {
		conn, err := listener.Accept()
		if err != nil {
			select {
			case <-ctx.Done():
				return
			default:
				logWarn("IPC accept failed: %v", err)
				continue
			}
		}

		mu.Lock()
		connID := *nextID
		*nextID++
		mu.Unlock()

		logInfo("Plugin connected (conn %d)", connID)

		go func(connID int, conn net.Conn) {
			defer func() {
				conn.Close()
				disconnectCh <- connID
				emitter.Emit("plugin:disconnected", nil)
			}()

			// Read role byte
			roleBuf := make([]byte, 1)
			if _, err := io.ReadFull(conn, roleBuf); err != nil {
				logWarn("Plugin (conn %d): failed to read role byte", connID)
				return
			}
			role := roleBuf[0]

			var streamIndex uint16
			if role != IPCRoleRecv {
				siBuf := make([]byte, 2)
				conn.SetReadDeadline(time.Now().Add(200 * time.Millisecond))
				if _, err := io.ReadFull(conn, siBuf); err == nil {
					streamIndex = binary.LittleEndian.Uint16(siBuf)
				}
				conn.SetReadDeadline(time.Time{})
			}

			if role == IPCRoleRecv {
				pool.Add(connID, conn)
				// Replay existing peer state so the recv plugin knows about
				// peers that joined before it connected.
				for _, snap := range peers.SnapshotForRecvReplay() {
					if err := WriteFrame(conn, EncodePeerJoinedMsg(snap.PeerID, snap.Identity)); err != nil {
						logWarn("Plugin (conn %d): failed to replay PeerJoined for %s: %v", connID, snap.PeerID, err)
					}
					if snap.DisplayName != "" {
						if err := WriteFrame(conn, EncodePeerNameMsg(snap.PeerID, snap.DisplayName)); err != nil {
							logWarn("Plugin (conn %d): failed to replay PeerName for %s: %v", connID, snap.PeerID, err)
						}
					}
				}
				logInfo("Plugin (conn %d) identified as recv", connID)
			} else {
				sendRegCh <- ipcSendRegistration{ConnID: connID, StreamIndex: streamIndex}
				logInfo("Plugin (conn %d) identified as send, stream_index=%d", connID, streamIndex)
			}
			emitter.Emit("plugin:connected", nil)

			// Read loop
			recvBuf := NewIPCRecvBuffer()
			buf := make([]byte, 65536)
			for {
				n, err := conn.Read(buf)
				if err != nil {
					logInfo("Plugin disconnected (conn %d)", connID)
					return
				}
				if role != IPCRoleRecv {
					recvBuf.Push(buf[:n])
					for {
						frame := recvBuf.NextFrame()
						if frame == nil {
							break
						}
						select {
						case fromPluginCh <- ipcFrame{connID: connID, data: frame}:
						default:
							dropCounter.Add(1)
						}
					}
				}
			}
		}(connID, conn)
	}
}

func handleIntervalBoundary(
	beat float64, bars uint32, quantum float64,
	lastIdx *int64, bpm float64, lastBoundary *time.Time,
	boundaryDriftUs **int64,
	mesh *PeerMesh,
	framesSent, framesRecv, bytesSent, bytesRecv *uint64,
	totalSent, totalRecv *uint64,
	lastIntervalIndex **int64, lastBoundaryTime **time.Time,
	testToneBoundaryCh chan IntervalBoundaryInfo,
	wavSenderBoundaryCh chan IntervalBoundaryInfo,
) {
	idx := computeIntervalIndex(beat, bars, quantum)
	if lastIdx != nil && idx <= *lastIdx {
		return
	}
	newIdx := idx
	*lastIntervalIndex = &newIdx

	log.Printf("[session] >>> INTERVAL %d <<< beat=%.1f sent=%d recv=%d", idx, beat, *framesSent, *framesRecv)
	*framesSent = 0
	*framesRecv = 0
	*bytesSent = 0
	*bytesRecv = 0

	if lastBoundary != nil {
		gap := time.Since(*lastBoundary)
		if bpm > 0 {
			beats := beatsPerInterval(bars, quantum)
			expectedUs := int64(beats / (bpm / 60.0) * 1_000_000.0)
			actualUs := gap.Microseconds()
			drift := actualUs - expectedUs
			*boundaryDriftUs = &drift
		}
	}
	now := time.Now()
	*lastBoundaryTime = &now

	mesh.Broadcast(NewIntervalBoundary(idx))

	info := IntervalBoundaryInfo{Index: idx, BPM: bpm, Bars: bars, Quantum: quantum}
	if testToneBoundaryCh != nil {
		select {
		case testToneBoundaryCh <- info:
		default:
		}
	}
	if wavSenderBoundaryCh != nil {
		select {
		case wavSenderBoundaryCh <- info:
		default:
		}
	}
}

func removePeerFully(peers *PeerRegistry, pool *IPCWriterPool, peerID string) {
	if !pool.IsEmpty() {
		pool.Broadcast(EncodeFrame(EncodePeerLeftMsg(peerID)))
	}
	peers.Remove(peerID)
}

func min64(a, b uint64) uint64 {
	if a < b {
		return a
	}
	return b
}

func pow2(n uint32) uint64 {
	if n > 30 {
		n = 30
	}
	return 1 << n
}
