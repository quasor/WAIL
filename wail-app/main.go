package main

import (
	"embed"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"

	"github.com/google/uuid"
	"github.com/honeybadger-io/honeybadger-go"
	"github.com/wailsapp/wails/v3/pkg/application"
)

//go:embed frontend/*
var assets embed.FS

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)

	// CLI flags
	room := flag.String("room", "", "Room to join on launch")
	bpmFlag := flag.Float64("bpm", 120.0, "BPM")
	name := flag.String("name", "", "Display name (auto-generated if empty)")
	password := flag.String("password", "", "Room password (optional)")
	headless := flag.Bool("headless", false, "Run without GUI (CLI mode)")
	wavFile := flag.String("wav", "", "WAV file to send (headless mode)")
	instance := flag.Int("instance", 0, "Instance number (port = 9191+N, separate data dir)")
	flag.Parse()

	// Initialize Honeybadger error reporting
	InitHoneybadger()
	defer honeybadger.Monitor()
	defer FlushHoneybadger()

	log.Println("WAIL - WebSocket Audio Interchange for Link (Go/Wails)")

	appBackend := NewApp(*instance)

	// Set up rotating file logger + WebSocket log broadcaster
	logDir := filepath.Join(appBackend.dataDir, "logs")
	fileWriter, wsLogWriter, err := SetupLogOutputs(logDir)
	if err != nil {
		log.Printf("Warning: file logging disabled: %v", err)
	} else {
		combined := io.MultiWriter(os.Stderr, fileWriter, wsLogWriter)
		log.SetOutput(combined)
		appBackend.fileLog = fileWriter
		appBackend.wsLog = wsLogWriter
	}

	log.Printf("App initialized — identity: %s, IPC port: %d", appBackend.identity, appBackend.ipcPort)

	if *headless {
		runHeadless(appBackend, *room, *password, *bpmFlag, *name, *wavFile)
		return
	}

	// --- GUI mode ---

	title := "WAIL"
	if *instance > 0 {
		title = fmt.Sprintf("WAIL (Instance %d)", *instance+1)
	}

	wailsApp := application.New(application.Options{
		Name: "WAIL",
		Services: []application.Service{
			application.NewService(appBackend),
		},
		Assets: application.AssetOptions{
			Handler: application.BundledAssetFileServer(assets),
		},
	})

	appBackend.SetEmitter(NewWailsEmitter(wailsApp))

	wailsApp.Window.NewWithOptions(application.WebviewWindowOptions{
		Title:  title,
		Width:  480,
		Height: 640,
		URL:    "/",
	})

	// Auto-join room if requested
	if *room != "" {
		displayName := *name
		if displayName == "" {
			displayName = fmt.Sprintf("test-%s", uuid.New().String()[:6])
		}
		log.Printf("Auto-joining room %q as %q at %.0f BPM", *room, displayName, *bpmFlag)
		bpm := *bpmFlag
		testMode := true
		var pw *string
		if *password != "" {
			pw = password
		}
		go func() {
			result, err := appBackend.JoinRoom(*room, pw, displayName, &bpm, nil, nil, nil, nil, nil, nil, nil, &testMode)
			if err != nil {
				log.Printf("Failed to auto-join room: %v", err)
				return
			}
			log.Printf("Joined room %q as peer %s", result.Room, result.PeerID)
		}()
	}

	err = wailsApp.Run()
	appBackend.Shutdown()
	if err != nil {
		log.Fatalf("Wails app error: %v", err)
	}
}

func runHeadless(app *App, room, password string, bpm float64, name, wavFile string) {
	if room == "" {
		log.Fatal("-room is required in headless mode")
	}

	app.SetEmitter(&NoopEmitter{})

	displayName := name
	if displayName == "" {
		displayName = fmt.Sprintf("wav-%s", uuid.New().String()[:6])
	}

	var pw *string
	if password != "" {
		pw = &password
	}
	testMode := true

	log.Printf("Headless mode: joining room %q as %q at %.0f BPM", room, displayName, bpm)

	result, err := app.JoinRoom(room, pw, displayName, &bpm, nil, nil, nil, nil, nil, nil, nil, &testMode)
	if err != nil {
		log.Fatalf("Failed to join room: %v", err)
	}
	log.Printf("Joined room %q as peer %s", result.Room, result.PeerID)

	if wavFile != "" {
		streamIdx := uint16(0)
		if err := app.SetWavSender(&streamIdx, wavFile); err != nil {
			log.Fatalf("Failed to start WAV sender: %v", err)
		}
		log.Printf("WAV sender started: %s", wavFile)
	}

	// Block until signal
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	sig := <-sigCh
	log.Printf("Received %s, shutting down...", sig)

	app.Shutdown()
}
