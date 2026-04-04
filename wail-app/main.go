package main

import (
	"embed"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"

	"github.com/google/uuid"
	"github.com/honeybadger-io/honeybadger-go"
	"github.com/wailsapp/wails/v3/pkg/application"
)

//go:embed frontend/*
var assets embed.FS

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)

	// CLI flags
	testRoom := flag.String("test-room", "", "Join a room in test mode on launch")
	testBPM := flag.Float64("test-bpm", 120.0, "BPM for test mode")
	testName := flag.String("test-name", "", "Display name for test mode participant")
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
		// Continue without file logging
	} else {
		combined := io.MultiWriter(os.Stderr, fileWriter, wsLogWriter)
		log.SetOutput(combined)
		appBackend.fileLog = fileWriter
		appBackend.wsLog = wsLogWriter
	}

	log.Printf("App initialized — identity: %s, IPC port: %d", appBackend.identity, appBackend.ipcPort)

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

	// Auto-join test room if requested
	if *testRoom != "" {
		displayName := *testName
		if displayName == "" {
			displayName = fmt.Sprintf("test-%s", uuid.New().String()[:6])
		}
		log.Printf("Auto-joining test room %q as %q at %.0f BPM", *testRoom, displayName, *testBPM)
		bpm := *testBPM
		testMode := true
		go func() {
			result, err := appBackend.JoinRoom(*testRoom, nil, displayName, &bpm, nil, nil, nil, nil, nil, nil, nil, &testMode)
			if err != nil {
				log.Printf("Failed to auto-join test room: %v", err)
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
