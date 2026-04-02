package main

import (
	"math"
	"testing"
)

func TestFrameWireNonFinalRoundtrip(t *testing.T) {
	frame := &AudioFrame{
		IntervalIndex: 42,
		StreamID:      3,
		FrameNumber:   7,
		Channels:      2,
		OpusData:      []byte{0xDE, 0xAD, 0xBE, 0xEF},
		IsFinal:       false,
	}

	encoded := EncodeAudioFrameWire(frame)
	if string(encoded[0:4]) != "WAIF" {
		t.Fatal("wrong magic")
	}
	if encoded[4] != frameFlagStereo {
		t.Fatalf("expected stereo flag, got 0x%02x", encoded[4])
	}
	if len(encoded) != 25 {
		t.Fatalf("expected 25 bytes, got %d", len(encoded))
	}

	decoded, err := DecodeAudioFrameWire(encoded)
	if err != nil {
		t.Fatalf("decode error: %v", err)
	}
	if decoded.IntervalIndex != 42 {
		t.Fatalf("expected interval 42, got %d", decoded.IntervalIndex)
	}
	if decoded.StreamID != 3 {
		t.Fatalf("expected stream 3, got %d", decoded.StreamID)
	}
	if decoded.FrameNumber != 7 {
		t.Fatalf("expected frame 7, got %d", decoded.FrameNumber)
	}
	if decoded.Channels != 2 {
		t.Fatalf("expected 2 channels, got %d", decoded.Channels)
	}
	if decoded.IsFinal {
		t.Fatal("should not be final")
	}
}

func TestFrameWireFinalRoundtrip(t *testing.T) {
	frame := &AudioFrame{
		IntervalIndex: 10,
		StreamID:      0,
		FrameNumber:   399,
		Channels:      1,
		OpusData:      []byte{0xAB},
		IsFinal:       true,
		SampleRate:    48000,
		TotalFrames:   400,
		BPM:           120.0,
		Quantum:       4.0,
		Bars:          4,
	}

	encoded := EncodeAudioFrameWire(frame)
	if len(encoded) != 50 {
		t.Fatalf("expected 50 bytes, got %d", len(encoded))
	}

	decoded, err := DecodeAudioFrameWire(encoded)
	if err != nil {
		t.Fatalf("decode error: %v", err)
	}
	if decoded.IntervalIndex != 10 || decoded.FrameNumber != 399 {
		t.Fatal("field mismatch")
	}
	if !decoded.IsFinal {
		t.Fatal("should be final")
	}
	if decoded.SampleRate != 48000 || decoded.TotalFrames != 400 {
		t.Fatal("metadata mismatch")
	}
	if math.Abs(decoded.BPM-120.0) > 1e-10 || math.Abs(decoded.Quantum-4.0) > 1e-10 {
		t.Fatal("float metadata mismatch")
	}
	if decoded.Bars != 4 {
		t.Fatalf("expected bars 4, got %d", decoded.Bars)
	}
}

func TestFrameWireRejectsBadMagic(t *testing.T) {
	data := make([]byte, 25)
	copy(data[0:4], "NOPE")
	if _, err := DecodeAudioFrameWire(data); err == nil {
		t.Fatal("should reject bad magic")
	}
}

func TestFrameWireRejectsTruncated(t *testing.T) {
	if _, err := DecodeAudioFrameWire(make([]byte, 10)); err == nil {
		t.Fatal("should reject short data")
	}
}

func TestPeekWaifHeaderNonFinal(t *testing.T) {
	frame := &AudioFrame{
		IntervalIndex: 42, StreamID: 3, FrameNumber: 7,
		Channels: 2, OpusData: []byte{0xDE, 0xAD}, IsFinal: false,
	}
	encoded := EncodeAudioFrameWire(frame)
	peek := PeekWaifHeader(encoded)
	if peek == nil {
		t.Fatal("peek returned nil")
	}
	if peek.IntervalIndex != 42 || peek.FrameNumber != 7 || peek.IsFinal {
		t.Fatal("peek mismatch")
	}
}

func TestPeekWaifHeaderFinal(t *testing.T) {
	frame := &AudioFrame{
		IntervalIndex: 10, StreamID: 0, FrameNumber: 49,
		Channels: 1, OpusData: []byte{0xAB}, IsFinal: true,
		SampleRate: 48000, TotalFrames: 50, BPM: 120.0, Quantum: 4.0, Bars: 4,
	}
	encoded := EncodeAudioFrameWire(frame)
	peek := PeekWaifHeader(encoded)
	if peek == nil {
		t.Fatal("peek returned nil")
	}
	if peek.IntervalIndex != 10 || !peek.IsFinal || peek.TotalFrames != 50 {
		t.Fatal("peek mismatch")
	}
}

func TestPeekWaifHeaderTooShort(t *testing.T) {
	if PeekWaifHeader(make([]byte, 10)) != nil {
		t.Fatal("should return nil for short data")
	}
}

func TestPeekWaifHeaderWrongMagic(t *testing.T) {
	data := make([]byte, 25)
	copy(data[0:4], "NOPE")
	if PeekWaifHeader(data) != nil {
		t.Fatal("should return nil for wrong magic")
	}
}

func TestRewriteWaifIntervalIndexRoundtrip(t *testing.T) {
	frame := &AudioFrame{
		IntervalIndex: 5, StreamID: 3, FrameNumber: 7,
		Channels: 2, OpusData: make([]byte, 100), IsFinal: false,
	}
	data := EncodeAudioFrameWire(frame)

	peek := PeekWaifHeader(data)
	if peek.IntervalIndex != 5 {
		t.Fatal("original index mismatch")
	}

	if !RewriteWaifIntervalIndex(data, 42) {
		t.Fatal("rewrite should succeed")
	}

	peek = PeekWaifHeader(data)
	if peek.IntervalIndex != 42 {
		t.Fatalf("expected 42 after rewrite, got %d", peek.IntervalIndex)
	}
	if peek.FrameNumber != 7 {
		t.Fatal("frame number should be unchanged")
	}

	// Full decode confirms data intact
	decoded, err := DecodeAudioFrameWire(data)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.IntervalIndex != 42 || decoded.StreamID != 3 {
		t.Fatal("full decode mismatch after rewrite")
	}
}

func TestRewriteWaifShortData(t *testing.T) {
	if RewriteWaifIntervalIndex(make([]byte, 10), 42) {
		t.Fatal("should fail for short data")
	}
}

func TestRewriteWaifWrongMagic(t *testing.T) {
	data := make([]byte, 25)
	copy(data[0:4], "NOPE")
	if RewriteWaifIntervalIndex(data, 42) {
		t.Fatal("should fail for wrong magic")
	}
}
