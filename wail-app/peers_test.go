package main

import (
	"testing"
	"time"
)

func strPtr(s string) *string { return &s }

func TestAddAndGetPeer(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", strPtr("Alice"))
	peer := reg.Get("peer1")
	if peer == nil || *peer.DisplayName != "Alice" {
		t.Fatal("expected Alice")
	}
	if peer.HelloSent {
		t.Fatal("hello should not be sent yet")
	}
}

func TestRemovePeerFreesSlot(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", strPtr("Alice"))
	reg.WithPeer("peer1", func(p *PeerState) {
		p.Identity = strPtr("alice-id")
	})
	slot := reg.AssignSlot("peer1", 0)
	if slot != 0 {
		t.Fatalf("expected slot 0, got %d", slot)
	}
	if !reg.IsSlotOccupied(0) {
		t.Fatal("slot should be occupied")
	}

	reg.Remove("peer1")
	if reg.IsSlotOccupied(0) {
		t.Fatal("slot should be freed")
	}
	if reg.Get("peer1") != nil {
		t.Fatal("peer should be removed")
	}
}

func TestSlotAffinityPreservedOnRejoin(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", strPtr("Alice"))
	reg.WithPeer("peer1", func(p *PeerState) {
		p.Identity = strPtr("alice-id")
	})
	slot := reg.AssignSlot("peer1", 0)
	if slot != 0 {
		t.Fatal("expected slot 0")
	}

	reg.Remove("peer1")

	// Rejoin with new peer_id, same identity
	reg.Add("peer2", strPtr("Alice"))
	reg.WithPeer("peer2", func(p *PeerState) {
		p.Identity = strPtr("alice-id")
	})
	slot2 := reg.AssignSlot("peer2", 0)
	if slot2 != 0 {
		t.Fatalf("should reclaim slot 0 via affinity, got %d", slot2)
	}
}

func TestTimedOutPeers(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", nil)
	reg.WithPeer("peer1", func(p *PeerState) {
		p.LastSeen = time.Now().Add(-60 * time.Second)
		p.EverReceivedMessage = true
	})
	reg.Add("peer2", nil) // fresh

	timedOut := reg.TimedOutPeers(30 * time.Second)
	found := false
	for _, id := range timedOut {
		if id == "peer1" {
			found = true
		}
		if id == "peer2" {
			t.Fatal("peer2 should not be timed out")
		}
	}
	if !found {
		t.Fatal("peer1 should be timed out")
	}
}

func TestTimedOutSkipsNeverConnectedPeers(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", nil)
	reg.WithPeer("peer1", func(p *PeerState) {
		p.LastSeen = time.Now().Add(-31 * time.Second)
		// EverReceivedMessage defaults to false
	})
	if len(reg.TimedOutPeers(30*time.Second)) > 0 {
		t.Fatal("should skip never-connected peers")
	}
}

func TestDeriveStatus(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", nil)
	if reg.DeriveStatus("peer1") != "connecting" {
		t.Fatal("should be connecting without display name")
	}
	reg.WithPeer("peer1", func(p *PeerState) {
		p.DisplayName = strPtr("Alice")
	})
	if reg.DeriveStatus("peer1") != "connected" {
		t.Fatal("should be connected with display name")
	}
	if reg.DeriveStatus("unknown") != "connecting" {
		t.Fatal("unknown should be connecting")
	}
}

func TestMarkHelloSentReturnsTrueOnce(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", nil)
	if !reg.MarkHelloSent("peer1") {
		t.Fatal("first call should return true")
	}
	if reg.MarkHelloSent("peer1") {
		t.Fatal("second call should return false")
	}
	reg.ClearHelloSent("peer1")
	if !reg.MarkHelloSent("peer1") {
		t.Fatal("after clear should return true")
	}
}

func TestRekeyPeerSlots(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", nil)
	slot := reg.AssignSlot("peer1", 0)
	if slot != 0 {
		t.Fatal("expected slot 0")
	}

	reg.WithPeer("peer1", func(p *PeerState) {
		p.Identity = strPtr("uuid-alice")
	})
	reg.RekeyPeerSlots("peer1", "uuid-alice")

	pid, found := reg.FindByIdentity("uuid-alice")
	if !found || pid != "peer1" {
		t.Fatal("should find peer1 by identity")
	}
}

func TestNoIdentityActivePeersSoftBucket(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", strPtr("Alice"))
	reg.WithPeer("peer1", func(p *PeerState) {
		p.EverReceivedMessage = true
		p.AddedAt = time.Now().Add(-6 * time.Second)
	})

	soft, hard := reg.NoIdentityActivePeers(5*time.Second, 15*time.Second)
	if len(soft) != 1 || soft[0] != "peer1" {
		t.Fatal("should be in soft bucket")
	}
	if len(hard) != 0 {
		t.Fatal("should not be in hard bucket")
	}

	reg.MarkHelloRetrySent("peer1")
	soft, _ = reg.NoIdentityActivePeers(5*time.Second, 15*time.Second)
	if len(soft) != 0 {
		t.Fatal("should not repeat after mark")
	}
}

func TestNoIdentityActivePeersHardBucket(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", strPtr("Alice"))
	reg.WithPeer("peer1", func(p *PeerState) {
		p.EverReceivedMessage = true
		p.AddedAt = time.Now().Add(-16 * time.Second)
	})

	soft, hard := reg.NoIdentityActivePeers(5*time.Second, 15*time.Second)
	if len(soft) != 0 {
		t.Fatal("should not be in soft bucket at 16s")
	}
	if len(hard) != 1 || hard[0] != "peer1" {
		t.Fatal("should be in hard bucket at 16s")
	}
}

func TestNoIdentityActivePeersExcludesIdentified(t *testing.T) {
	reg := NewPeerRegistry()
	reg.Add("peer1", strPtr("Alice"))
	reg.WithPeer("peer1", func(p *PeerState) {
		p.EverReceivedMessage = true
		p.Identity = strPtr("alice-uuid")
		p.AddedAt = time.Now().Add(-20 * time.Second)
	})

	soft, hard := reg.NoIdentityActivePeers(5*time.Second, 15*time.Second)
	if len(soft) != 0 || len(hard) != 0 {
		t.Fatal("identified peers must be excluded")
	}
}
