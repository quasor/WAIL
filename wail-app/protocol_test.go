package main

import (
	"encoding/json"
	"testing"
)

func TestSyncMessageHelloRoundtrip(t *testing.T) {
	dn := "Ringo"
	id := "stable-uuid-1234"
	msg := NewHello("abc123", &dn, &id)
	data, err := json.Marshal(msg)
	if err != nil {
		t.Fatal(err)
	}
	var decoded SyncMessage
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatal(err)
	}
	if decoded.Type != "Hello" || decoded.PeerID != "abc123" {
		t.Fatal("field mismatch")
	}
	if decoded.DisplayName == nil || *decoded.DisplayName != "Ringo" {
		t.Fatal("display name mismatch")
	}
	if decoded.Identity == nil || *decoded.Identity != "stable-uuid-1234" {
		t.Fatal("identity mismatch")
	}
}

func TestSyncMessageIntervalBoundaryRoundtrip(t *testing.T) {
	msg := NewIntervalBoundary(42)
	data, _ := json.Marshal(msg)
	var decoded SyncMessage
	json.Unmarshal(data, &decoded)
	if decoded.Type != "IntervalBoundary" || decoded.Index != 42 {
		t.Fatal("mismatch")
	}
}

func TestStreamNamesToWireRoundtrip(t *testing.T) {
	names := map[uint16]string{0: "Bass", 1: "Drums"}
	wire := StreamNamesToWire(names)
	if wire["0"] != "Bass" || wire["1"] != "Drums" {
		t.Fatal("wire conversion failed")
	}
	back := StreamNamesFromWire(wire)
	if back[0] != "Bass" || back[1] != "Drums" {
		t.Fatal("round-trip failed")
	}
}

func TestChatMessageRoundtrip(t *testing.T) {
	msg := NewChatMessage("Ringo", "Let's change key")
	data, _ := json.Marshal(msg)
	var decoded SyncMessage
	json.Unmarshal(data, &decoded)
	if decoded.Type != "ChatMessage" || decoded.SenderName != "Ringo" || decoded.Text != "Let's change key" {
		t.Fatal("mismatch")
	}
}

func TestAudioStatusRoundtrip(t *testing.T) {
	msg := NewAudioStatus(true, 5, 3, true, 42)
	data, _ := json.Marshal(msg)
	var decoded SyncMessage
	json.Unmarshal(data, &decoded)
	if decoded.Type != "AudioStatus" || !decoded.AudioDCOpen || decoded.IntervalsSent != 5 {
		t.Fatal("mismatch")
	}
	if decoded.Seq != 42 {
		t.Fatalf("expected seq 42, got %d", decoded.Seq)
	}
}
