package main

import (
	"encoding/binary"
	"io"
	"net"
	"sync"
)

// IPC role bytes sent by plugins on connect.
const (
	IPCRoleSend byte = 0x00
	IPCRoleRecv byte = 0x01
)

// IPC message tags.
const (
	IPCTagAudio      byte = 0x01
	IPCTagPeerJoined byte = 0x02
	IPCTagPeerLeft   byte = 0x03
	IPCTagPeerName   byte = 0x04
	IPCTagAudioFrame byte = 0x05
	IPCTagMetrics    byte = 0x06
)

// EncodeFrame wraps a payload in a length-prefixed IPC frame: [u32 LE length][payload].
func EncodeFrame(payload []byte) []byte {
	frame := make([]byte, 4+len(payload))
	binary.LittleEndian.PutUint32(frame[:4], uint32(len(payload)))
	copy(frame[4:], payload)
	return frame
}

// DecodeFrame tries to extract a complete frame from a buffer.
// Returns (payload, consumed, ok). If ok is false, more data is needed.
func DecodeFrame(buf []byte) ([]byte, int, bool) {
	if len(buf) < 4 {
		return nil, 0, false
	}
	payloadLen := int(binary.LittleEndian.Uint32(buf[:4]))
	total := 4 + payloadLen
	if len(buf) < total {
		return nil, 0, false
	}
	payload := make([]byte, payloadLen)
	copy(payload, buf[4:total])
	return payload, total, true
}

// IPCRecvBuffer accumulates bytes from socket reads and yields complete frames.
type IPCRecvBuffer struct {
	buf []byte
}

// NewIPCRecvBuffer creates a new receive buffer.
func NewIPCRecvBuffer() *IPCRecvBuffer {
	return &IPCRecvBuffer{buf: make([]byte, 0, 64*1024)}
}

// Push appends received bytes.
func (r *IPCRecvBuffer) Push(data []byte) {
	r.buf = append(r.buf, data...)
}

// NextFrame tries to extract the next complete frame. Returns nil if more data needed.
func (r *IPCRecvBuffer) NextFrame() []byte {
	payload, consumed, ok := DecodeFrame(r.buf)
	if !ok {
		return nil
	}
	r.buf = r.buf[consumed:]
	return payload
}

// Buffered returns the number of unconsumed bytes.
func (r *IPCRecvBuffer) Buffered() int {
	return len(r.buf)
}

// --- IPC Message encoding/decoding ---

// EncodeAudioMsg encodes an AudioInterval IPC message: tag + peer_id_len + peer_id + wire_data.
func EncodeAudioMsg(peerID string, wireData []byte) []byte {
	pid := []byte(peerID)
	pidLen := len(pid)
	if pidLen > 255 {
		pidLen = 255
	}
	msg := make([]byte, 2+pidLen+len(wireData))
	msg[0] = IPCTagAudio
	msg[1] = byte(pidLen)
	copy(msg[2:], pid[:pidLen])
	copy(msg[2+pidLen:], wireData)
	return msg
}

// DecodeAudioMsg decodes an AudioInterval IPC message. Returns (peerID, wireData, ok).
func DecodeAudioMsg(payload []byte) (string, []byte, bool) {
	if len(payload) < 2 || payload[0] != IPCTagAudio {
		return "", nil, false
	}
	pidLen := int(payload[1])
	if len(payload) < 2+pidLen {
		return "", nil, false
	}
	peerID := string(payload[2 : 2+pidLen])
	wireData := make([]byte, len(payload)-(2+pidLen))
	copy(wireData, payload[2+pidLen:])
	return peerID, wireData, true
}

// EncodePeerJoinedMsg encodes a PeerJoined IPC message.
func EncodePeerJoinedMsg(peerID, identity string) []byte {
	pid := []byte(peerID)
	pidLen := min8(len(pid), 255)
	ident := []byte(identity)
	identLen := min8(len(ident), 255)
	msg := make([]byte, 3+pidLen+identLen)
	msg[0] = IPCTagPeerJoined
	msg[1] = byte(pidLen)
	copy(msg[2:], pid[:pidLen])
	msg[2+pidLen] = byte(identLen)
	copy(msg[3+pidLen:], ident[:identLen])
	return msg
}

// DecodePeerJoinedMsg decodes a PeerJoined IPC message. Returns (peerID, identity, ok).
func DecodePeerJoinedMsg(payload []byte) (string, string, bool) {
	if len(payload) < 2 || payload[0] != IPCTagPeerJoined {
		return "", "", false
	}
	pidLen := int(payload[1])
	if len(payload) < 2+pidLen+1 {
		return "", "", false
	}
	peerID := string(payload[2 : 2+pidLen])
	identStart := 2 + pidLen
	identLen := int(payload[identStart])
	if len(payload) < identStart+1+identLen {
		return "", "", false
	}
	identity := string(payload[identStart+1 : identStart+1+identLen])
	return peerID, identity, true
}

// EncodePeerLeftMsg encodes a PeerLeft IPC message.
func EncodePeerLeftMsg(peerID string) []byte {
	pid := []byte(peerID)
	pidLen := min8(len(pid), 255)
	msg := make([]byte, 2+pidLen)
	msg[0] = IPCTagPeerLeft
	msg[1] = byte(pidLen)
	copy(msg[2:], pid[:pidLen])
	return msg
}

// DecodePeerLeftMsg decodes a PeerLeft IPC message. Returns (peerID, ok).
func DecodePeerLeftMsg(payload []byte) (string, bool) {
	if len(payload) < 2 || payload[0] != IPCTagPeerLeft {
		return "", false
	}
	pidLen := int(payload[1])
	if len(payload) < 2+pidLen {
		return "", false
	}
	return string(payload[2 : 2+pidLen]), true
}

// EncodePeerNameMsg encodes a PeerName IPC message.
func EncodePeerNameMsg(peerID, displayName string) []byte {
	pid := []byte(peerID)
	pidLen := min8(len(pid), 255)
	name := []byte(displayName)
	nameLen := min8(len(name), 255)
	msg := make([]byte, 3+pidLen+nameLen)
	msg[0] = IPCTagPeerName
	msg[1] = byte(pidLen)
	copy(msg[2:], pid[:pidLen])
	msg[2+pidLen] = byte(nameLen)
	copy(msg[3+pidLen:], name[:nameLen])
	return msg
}

// DecodePeerNameMsg decodes a PeerName IPC message. Returns (peerID, displayName, ok).
func DecodePeerNameMsg(payload []byte) (string, string, bool) {
	if len(payload) < 2 || payload[0] != IPCTagPeerName {
		return "", "", false
	}
	pidLen := int(payload[1])
	if len(payload) < 2+pidLen+1 {
		return "", "", false
	}
	peerID := string(payload[2 : 2+pidLen])
	nameStart := 2 + pidLen
	nameLen := int(payload[nameStart])
	if len(payload) < nameStart+1+nameLen {
		return "", "", false
	}
	displayName := string(payload[nameStart+1 : nameStart+1+nameLen])
	return peerID, displayName, true
}

// EncodeAudioFrameMsg encodes a streaming audio frame IPC message (no peer_id).
func EncodeAudioFrameMsg(wireData []byte) []byte {
	msg := make([]byte, 1+len(wireData))
	msg[0] = IPCTagAudioFrame
	copy(msg[1:], wireData)
	return msg
}

// DecodeAudioFrameMsg decodes a streaming audio frame IPC message.
func DecodeAudioFrameMsg(payload []byte) ([]byte, bool) {
	if len(payload) < 2 || payload[0] != IPCTagAudioFrame {
		return nil, false
	}
	wireData := make([]byte, len(payload)-1)
	copy(wireData, payload[1:])
	return wireData, true
}

// EncodeMetricsMsg encodes a plugin metrics report.
func EncodeMetricsMsg(decodeFailures uint64) []byte {
	msg := make([]byte, 9)
	msg[0] = IPCTagMetrics
	binary.LittleEndian.PutUint64(msg[1:], decodeFailures)
	return msg
}

// DecodeMetricsMsg decodes a plugin metrics report. Returns (decodeFailures, ok).
func DecodeMetricsMsg(payload []byte) (uint64, bool) {
	if len(payload) < 9 || payload[0] != IPCTagMetrics {
		return 0, false
	}
	return binary.LittleEndian.Uint64(payload[1:9]), true
}

// IPCTag returns the tag byte from a payload, or -1 if empty.
func IPCTag(payload []byte) int {
	if len(payload) == 0 {
		return -1
	}
	return int(payload[0])
}

func min8(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// IPCWriterPool manages TCP write connections to recv plugins.
type IPCWriterPool struct {
	mu      sync.Mutex
	writers map[int]net.Conn
}

// NewIPCWriterPool creates a new writer pool.
func NewIPCWriterPool() *IPCWriterPool {
	return &IPCWriterPool{writers: make(map[int]net.Conn)}
}

// Add adds a recv plugin connection.
func (p *IPCWriterPool) Add(connID int, conn net.Conn) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.writers[connID] = conn
}

// Remove removes a connection by ID.
func (p *IPCWriterPool) Remove(connID int) {
	p.mu.Lock()
	defer p.mu.Unlock()
	delete(p.writers, connID)
}

// IsEmpty returns true if no recv plugins are connected.
func (p *IPCWriterPool) IsEmpty() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.writers) == 0
}

// Len returns the number of active connections.
func (p *IPCWriterPool) Len() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.writers)
}

// Broadcast sends a frame to all recv plugins. Dead connections are removed.
func (p *IPCWriterPool) Broadcast(frame []byte) {
	p.mu.Lock()
	defer p.mu.Unlock()
	var dead []int
	for id, conn := range p.writers {
		if _, err := conn.Write(frame); err != nil {
			dead = append(dead, id)
		}
	}
	for _, id := range dead {
		delete(p.writers, id)
	}
}

// WriteFrame encodes a payload as a framed message and writes it to a connection.
func WriteFrame(w io.Writer, payload []byte) error {
	frame := EncodeFrame(payload)
	_, err := w.Write(frame)
	return err
}
