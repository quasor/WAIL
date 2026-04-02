package main

import (
	"flag"
	"fmt"
	"log"

	"github.com/google/uuid"
)

// StdoutEmitter is a simple EventEmitter that logs events to stdout.
// Will be replaced by WailsEmitter in Phase 3.
type StdoutEmitter struct{}

func (e *StdoutEmitter) Emit(event string, data any) {
	log.Printf("[event] %s: %+v", event, data)
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)

	// CLI flags
	testRoom := flag.String("test-room", "", "Join a room in test mode on launch")
	testBPM := flag.Float64("test-bpm", 120.0, "BPM for test mode")
	testName := flag.String("test-name", "", "Display name for test mode participant")
	instance := flag.Int("instance", 0, "Instance number (port = 9191+N, separate data dir)")
	flag.Parse()

	log.Println("WAIL - WebSocket Audio Interchange for Link (Go)")

	app := NewApp(*instance)
	app.SetEmitter(&StdoutEmitter{})

	log.Printf("App initialized — identity: %s, IPC port: %d", app.identity, app.ipcPort)

	// Auto-join test room if requested
	if *testRoom != "" {
		displayName := *testName
		if displayName == "" {
			displayName = fmt.Sprintf("test-%s", uuid.New().String()[:6])
		}
		log.Printf("Auto-joining test room %q as %q at %.0f BPM", *testRoom, displayName, *testBPM)
		bpm := *testBPM
		testMode := true
		result, err := app.JoinRoom(*testRoom, nil, displayName, &bpm, nil, nil, nil, nil, nil, nil, nil, &testMode)
		if err != nil {
			log.Fatalf("Failed to auto-join test room: %v", err)
		}
		log.Printf("Joined room %q as peer %s", result.Room, result.PeerID)
		// Block forever (session runs in goroutine)
		select {}
	}

	// TODO: Phase 3 replaces this with wails.Run(...)
	log.Println("Build OK — all Go types and session logic compiled successfully.")
}
