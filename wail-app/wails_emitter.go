package main

import (
	"log"
	"sync/atomic"

	"github.com/wailsapp/wails/v3/pkg/application"
)

// WailsEmitter implements EventEmitter using the Wails v3 event system.
type WailsEmitter struct {
	app    *application.App
	closed atomic.Bool
}

// NewWailsEmitter creates a new Wails event emitter.
func NewWailsEmitter(app *application.App) *WailsEmitter {
	return &WailsEmitter{app: app}
}

// Emit sends an event to the frontend. No-op after Shutdown.
func (e *WailsEmitter) Emit(event string, data any) {
	if e.closed.Load() {
		log.Printf("[emitter] Suppressed %s (shut down)", event)
		return
	}
	e.app.Event.Emit(event, data)
}

// Shutdown marks the emitter as closed so future Emit calls are no-ops.
// This prevents ExecJS calls on a closed webview window.
func (e *WailsEmitter) Shutdown() {
	e.closed.Store(true)
}
