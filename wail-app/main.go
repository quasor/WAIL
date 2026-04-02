package main

import (
	"log"
)

// StdoutEmitter is a simple EventEmitter that logs events to stdout.
// In production, this would be replaced by the Wails event system.
type StdoutEmitter struct{}

func (e *StdoutEmitter) Emit(event string, data any) {
	log.Printf("[event] %s: %+v", event, data)
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)
	log.Println("WAIL - WebSocket Audio Interchange for Link (Go)")
	log.Println("This is the Go/Wails evaluation build.")

	app := NewApp()
	app.SetEmitter(&StdoutEmitter{})

	// For the evaluation build, we just verify the app initializes correctly.
	// In production, this would be:
	//   wails.Run(&options.App{
	//       Title: "WAIL",
	//       Width: 480,
	//       Height: 640,
	//       Bind: []interface{}{app},
	//       OnStartup: app.SetEmitter,
	//   })
	log.Printf("App initialized with identity: %s", app.identity)
	log.Printf("IPC port: %d", app.ipcPort)
	log.Println("Build OK — all Go types and session logic compiled successfully.")
}
