package main

import (
	"encoding/binary"
	"testing"
)

// --- Frame encoding/decoding ---

func TestEncodeFramePrependsLength(t *testing.T) {
	payload := []byte{1, 2, 3, 4, 5}
	frame := EncodeFrame(payload)
	if len(frame) != 9 {
		t.Fatalf("expected 9 bytes, got %d", len(frame))
	}
	length := binary.LittleEndian.Uint32(frame[:4])
	if length != 5 {
		t.Fatalf("expected length 5, got %d", length)
	}
	for i, b := range payload {
		if frame[4+i] != b {
			t.Fatalf("payload mismatch at %d", i)
		}
	}
}

func TestEncodeEmptyPayload(t *testing.T) {
	frame := EncodeFrame([]byte{})
	if len(frame) != 4 {
		t.Fatalf("expected 4 bytes, got %d", len(frame))
	}
	if binary.LittleEndian.Uint32(frame[:4]) != 0 {
		t.Fatal("expected zero length")
	}
}

func TestDecodeFrameFromCompleteBuffer(t *testing.T) {
	payload := []byte{10, 20, 30}
	frame := EncodeFrame(payload)
	decoded, consumed, ok := DecodeFrame(frame)
	if !ok {
		t.Fatal("expected ok")
	}
	if consumed != 7 {
		t.Fatalf("expected consumed 7, got %d", consumed)
	}
	if len(decoded) != 3 || decoded[0] != 10 || decoded[1] != 20 || decoded[2] != 30 {
		t.Fatalf("payload mismatch: %v", decoded)
	}
}

func TestDecodeReturnsNilForIncompleteHeader(t *testing.T) {
	_, _, ok := DecodeFrame([]byte{0, 0})
	if ok {
		t.Fatal("should return not-ok for short buffer")
	}
}

func TestDecodeReturnsNilForIncompletePayload(t *testing.T) {
	buf := make([]byte, 7)
	binary.LittleEndian.PutUint32(buf[:4], 10)
	buf[4] = 1
	buf[5] = 2
	buf[6] = 3
	_, _, ok := DecodeFrame(buf)
	if ok {
		t.Fatal("should return not-ok for incomplete payload")
	}
}

// --- IPCRecvBuffer ---

func TestRecvBufferYieldsCompleteFrame(t *testing.T) {
	buf := NewIPCRecvBuffer()
	frame := EncodeFrame([]byte{1, 2, 3})
	buf.Push(frame)
	payload := buf.NextFrame()
	if payload == nil {
		t.Fatal("expected frame")
	}
	if len(payload) != 3 {
		t.Fatalf("expected 3 bytes, got %d", len(payload))
	}
	if buf.Buffered() != 0 {
		t.Fatalf("expected 0 buffered, got %d", buf.Buffered())
	}
}

func TestRecvBufferHandlesPartialDelivery(t *testing.T) {
	buf := NewIPCRecvBuffer()
	frame := EncodeFrame([]byte{10, 20, 30, 40, 50})
	buf.Push(frame[:3])
	if buf.NextFrame() != nil {
		t.Fatal("should not yield partial frame")
	}
	buf.Push(frame[3:])
	payload := buf.NextFrame()
	if payload == nil {
		t.Fatal("expected frame after completing")
	}
	if len(payload) != 5 {
		t.Fatalf("expected 5, got %d", len(payload))
	}
}

func TestRecvBufferMultipleFramesInOnePush(t *testing.T) {
	buf := NewIPCRecvBuffer()
	f1 := EncodeFrame([]byte{0xAA})
	f2 := EncodeFrame([]byte{0xBB, 0xCC})
	combined := append(f1, f2...)
	buf.Push(combined)

	p1 := buf.NextFrame()
	if p1 == nil || len(p1) != 1 || p1[0] != 0xAA {
		t.Fatal("first frame mismatch")
	}
	p2 := buf.NextFrame()
	if p2 == nil || len(p2) != 2 || p2[0] != 0xBB {
		t.Fatal("second frame mismatch")
	}
	if buf.NextFrame() != nil {
		t.Fatal("should be empty")
	}
}

// --- IPC Message encoding ---

func TestAudioMsgRoundtrip(t *testing.T) {
	wireData := []byte{0x01, 0x02, 0x03, 0x04}
	encoded := EncodeAudioMsg("peer-abc", wireData)
	peerID, decoded, ok := DecodeAudioMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if peerID != "peer-abc" {
		t.Fatalf("expected peer-abc, got %s", peerID)
	}
	if len(decoded) != 4 {
		t.Fatalf("expected 4 bytes, got %d", len(decoded))
	}
}

func TestAudioMsgEmptyPeerID(t *testing.T) {
	wireData := []byte{0xAA, 0xBB}
	encoded := EncodeAudioMsg("", wireData)
	peerID, decoded, ok := DecodeAudioMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if peerID != "" {
		t.Fatalf("expected empty, got %s", peerID)
	}
	if len(decoded) != 2 {
		t.Fatalf("expected 2, got %d", len(decoded))
	}
}

func TestAudioMsgRejectsShort(t *testing.T) {
	if _, _, ok := DecodeAudioMsg([]byte{}); ok {
		t.Fatal("should reject empty")
	}
	if _, _, ok := DecodeAudioMsg([]byte{0x01}); ok {
		t.Fatal("should reject single byte")
	}
}

func TestAudioMsgRejectsWrongTag(t *testing.T) {
	if _, _, ok := DecodeAudioMsg([]byte{0xFF, 0x00}); ok {
		t.Fatal("should reject wrong tag")
	}
}

func TestPeerJoinedRoundtrip(t *testing.T) {
	encoded := EncodePeerJoinedMsg("peer-abc", "uuid-1234")
	peerID, identity, ok := DecodePeerJoinedMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if peerID != "peer-abc" || identity != "uuid-1234" {
		t.Fatalf("mismatch: %s / %s", peerID, identity)
	}
}

func TestPeerLeftRoundtrip(t *testing.T) {
	encoded := EncodePeerLeftMsg("peer-xyz")
	peerID, ok := DecodePeerLeftMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if peerID != "peer-xyz" {
		t.Fatalf("expected peer-xyz, got %s", peerID)
	}
}

func TestPeerNameRoundtrip(t *testing.T) {
	encoded := EncodePeerNameMsg("peer-abc", "Ringo")
	peerID, name, ok := DecodePeerNameMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if peerID != "peer-abc" || name != "Ringo" {
		t.Fatalf("mismatch: %s / %s", peerID, name)
	}
}

func TestAudioFrameMsgRoundtrip(t *testing.T) {
	wireData := []byte{0xDE, 0xAD, 0xBE, 0xEF}
	encoded := EncodeAudioFrameMsg(wireData)
	if encoded[0] != IPCTagAudioFrame {
		t.Fatal("wrong tag")
	}
	decoded, ok := DecodeAudioFrameMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if len(decoded) != 4 || decoded[0] != 0xDE {
		t.Fatal("data mismatch")
	}
}

func TestMetricsMsgRoundtrip(t *testing.T) {
	encoded := EncodeMetricsMsg(42)
	if encoded[0] != IPCTagMetrics || len(encoded) != 9 {
		t.Fatal("wrong encoding")
	}
	val, ok := DecodeMetricsMsg(encoded)
	if !ok {
		t.Fatal("decode failed")
	}
	if val != 42 {
		t.Fatalf("expected 42, got %d", val)
	}
}

func TestIPCTag(t *testing.T) {
	if IPCTag([]byte{IPCTagAudio, 0x00}) != int(IPCTagAudio) {
		t.Fatal("wrong tag for Audio")
	}
	if IPCTag([]byte{}) != -1 {
		t.Fatal("should return -1 for empty")
	}
}
