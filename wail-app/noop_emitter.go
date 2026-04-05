package main

import "log"

// NoopEmitter implements EventEmitter without a GUI frontend.
// Used in headless CLI mode.
type NoopEmitter struct{}

func (e *NoopEmitter) Emit(event string, data any) {
	log.Printf("[event] %s", event)
}

func (e *NoopEmitter) Shutdown() {}
