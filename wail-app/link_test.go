package main

import (
	"math"
	"testing"
	"time"
)

func TestAboveThresholdEmitsChange(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	bpm, changed := d.Check(120.02, now)
	if !changed || bpm != 120.02 {
		t.Fatal("expected change")
	}
}

func TestBelowThresholdIgnored(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	_, changed := d.Check(120.005, now)
	if changed {
		t.Fatal("should not change below threshold")
	}
}

func TestEchoGuardSuppressesDetection(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	d.ArmEchoGuard(now.Add(150 * time.Millisecond))
	_, changed := d.Check(130.0, now)
	if changed {
		t.Fatal("should suppress during echo guard")
	}
	_, changed = d.Check(130.0, now.Add(100*time.Millisecond))
	if changed {
		t.Fatal("should still suppress at 100ms")
	}
}

func TestEchoGuardExpiresAllowsDetection(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	d.ArmEchoGuard(now.Add(150 * time.Millisecond))
	bpm, changed := d.Check(130.0, now.Add(151*time.Millisecond))
	if !changed || bpm != 130.0 {
		t.Fatal("should detect after guard expires")
	}
}

func TestEchoGuardClearsAfterExpiry(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	d.ArmEchoGuard(now.Add(150 * time.Millisecond))
	d.Check(130.0, now.Add(200*time.Millisecond))
	bpm, changed := d.Check(140.0, now.Add(210*time.Millisecond))
	if !changed || bpm != 140.0 {
		t.Fatal("guard should be cleared, second change should work")
	}
}

func TestLastTempoTracksAcrossChanges(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	d.Check(125.0, now)
	if d.LastTempo() != 125.0 {
		t.Fatal("expected 125.0")
	}
	d.Check(130.0, now)
	if d.LastTempo() != 130.0 {
		t.Fatal("expected 130.0")
	}
	d.Check(130.005, now)
	if d.LastTempo() != 130.0 {
		t.Fatal("below threshold should not update baseline")
	}
}

func TestNaNBPMDoesNotPoisonDetector(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	d.SetLastTempo(math.NaN())
	bpm, changed := d.Check(130.0, now)
	if !changed || bpm != 130.0 {
		t.Fatal("NaN should not poison detector — must still detect changes")
	}
}

func TestZeroBPMRejectedByDetector(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	_, changed := d.Check(0.0, now)
	if changed {
		t.Fatal("zero BPM should be rejected")
	}
	if d.LastTempo() != 120.0 {
		t.Fatal("baseline should not change")
	}
}

func TestNegativeBPMRejectedByDetector(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	now := time.Now()
	_, changed := d.Check(-120.0, now)
	if changed {
		t.Fatal("negative BPM should be rejected")
	}
	if d.LastTempo() != 120.0 {
		t.Fatal("baseline should not change")
	}
}

func TestSetLastTempoRejectsInvalid(t *testing.T) {
	d := NewTempoChangeDetector(120.0)
	d.SetLastTempo(math.NaN())
	if d.LastTempo() != 120.0 {
		t.Fatal("NaN should be rejected by SetLastTempo")
	}
	d.SetLastTempo(0.0)
	if d.LastTempo() != 120.0 {
		t.Fatal("zero should be rejected by SetLastTempo")
	}
	d.SetLastTempo(-50.0)
	if d.LastTempo() != 120.0 {
		t.Fatal("negative should be rejected by SetLastTempo")
	}
}
