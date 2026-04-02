package main

import (
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"time"
)

// RecordingConfig holds configuration for local session recording.
type RecordingConfig struct {
	Enabled       bool   `json:"enabled"`
	Directory     string `json:"directory"`
	Stems         bool   `json:"stems"`
	RetentionDays uint32 `json:"retention_days"`
}

// RecordCommand is sent from the session loop to the recorder.
type RecordCommand struct {
	Type        string // "PeerInterval" or "Finalize"
	PeerID      string
	DisplayName *string
	WireData    []byte
}

// SessionRecorder manages recording for a single session.
// Note: In the Go port, we only record raw WAIF frames (no Opus decode).
// Full decode+WAV writing requires the Opus codec which lives in the Rust plugins.
// For the evaluation, we record WAIF frames to a binary log.
type SessionRecorder struct {
	cmdCh        chan RecordCommand
	bytesWritten atomic.Uint64
}

// StartRecording starts a new recording session.
func StartRecording(config RecordingConfig, room string) (*SessionRecorder, error) {
	if config.RetentionDays > 0 {
		if _, _, err := CleanupOldSessions(config.Directory, config.RetentionDays); err != nil {
			log.Printf("[recorder] Cleanup failed: %v", err)
		}
	}

	sessionDir, err := createSessionDir(config.Directory, room)
	if err != nil {
		return nil, err
	}

	rec := &SessionRecorder{
		cmdCh: make(chan RecordCommand, 256),
	}

	log.Printf("[recorder] Recording session to %s", sessionDir)

	go func() {
		// Simple frame log recorder
		logPath := filepath.Join(sessionDir, "frames.waif")
		f, err := os.Create(logPath)
		if err != nil {
			log.Printf("[recorder] Failed to create frame log: %v", err)
			return
		}
		defer f.Close()

		var totalBytes uint64
		for cmd := range rec.cmdCh {
			switch cmd.Type {
			case "PeerInterval":
				// Write: [4 bytes peer_id_len][peer_id][4 bytes data_len][data]
				pidBytes := []byte(cmd.PeerID)
				header := make([]byte, 8)
				header[0] = byte(len(pidBytes))
				header[1] = byte(len(pidBytes) >> 8)
				header[2] = byte(len(pidBytes) >> 16)
				header[3] = byte(len(pidBytes) >> 24)
				header[4] = byte(len(cmd.WireData))
				header[5] = byte(len(cmd.WireData) >> 8)
				header[6] = byte(len(cmd.WireData) >> 16)
				header[7] = byte(len(cmd.WireData) >> 24)
				f.Write(header)
				f.Write(pidBytes)
				f.Write(cmd.WireData)
				totalBytes += uint64(8 + len(pidBytes) + len(cmd.WireData))
				rec.bytesWritten.Store(totalBytes)
			case "Finalize":
				log.Printf("[recorder] Finalized recording (%d bytes)", totalBytes)
				return
			}
		}
	}()

	return rec, nil
}

// RecordPeer records a WAIF frame from a peer.
func (r *SessionRecorder) RecordPeer(peerID string, displayName *string, wireData []byte) {
	select {
	case r.cmdCh <- RecordCommand{Type: "PeerInterval", PeerID: peerID, DisplayName: displayName, WireData: wireData}:
	default:
		log.Printf("[recorder] Channel full, dropping frame from %s", peerID)
	}
}

// Finalize stops the recording.
func (r *SessionRecorder) Finalize() {
	select {
	case r.cmdCh <- RecordCommand{Type: "Finalize"}:
	default:
	}
}

// BytesWritten returns the total bytes written.
func (r *SessionRecorder) BytesWritten() uint64 {
	return r.bytesWritten.Load()
}

func createSessionDir(base, room string) (string, error) {
	timestamp := time.Now().Format("2006-01-02_15-04-05")
	safeRoom := sanitizeFilename(room)
	dirName := fmt.Sprintf("%s_%s", timestamp, safeRoom)
	dir := filepath.Join(base, dirName)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	return dir, nil
}

func sanitizeFilename(s string) string {
	var b strings.Builder
	for i, c := range s {
		if i >= 32 {
			break
		}
		if (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '_' {
			b.WriteRune(c)
		} else {
			b.WriteRune('_')
		}
	}
	return b.String()
}

// CleanupOldSessions deletes recording sessions older than retentionDays.
func CleanupOldSessions(baseDir string, retentionDays uint32) (uint32, uint64, error) {
	if retentionDays == 0 {
		return 0, 0, nil
	}
	cutoff := time.Now().AddDate(0, 0, -int(retentionDays))

	entries, err := os.ReadDir(baseDir)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, 0, nil
		}
		return 0, 0, err
	}

	var deleted uint32
	var freed uint64
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		name := entry.Name()
		if len(name) < 19 {
			continue
		}
		dateStr := name[:19]
		t, err := time.Parse("2006-01-02_15-04-05", dateStr)
		if err != nil {
			continue
		}
		if t.Before(cutoff) {
			path := filepath.Join(baseDir, name)
			size := dirSize(path)
			if err := os.RemoveAll(path); err != nil {
				log.Printf("[recorder] Failed to delete %s: %v", path, err)
				continue
			}
			deleted++
			freed += size
		}
	}
	return deleted, freed, nil
}

func dirSize(path string) uint64 {
	var size uint64
	filepath.Walk(path, func(_ string, info os.FileInfo, _ error) error {
		if info != nil && !info.IsDir() {
			size += uint64(info.Size())
		}
		return nil
	})
	return size
}

// DefaultRecordingDir returns the platform-appropriate default recording directory.
func DefaultRecordingDir() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, "Music", "WAIL Sessions"), nil
}
