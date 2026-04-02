package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestLoadEmptyDirReturnsEmpty(t *testing.T) {
	dir := t.TempDir()
	names := LoadStreamNames(dir)
	if len(names) != 0 {
		t.Fatalf("expected empty, got %d", len(names))
	}
}

func TestSaveAndLoadRoundtrip(t *testing.T) {
	dir := t.TempDir()
	names := map[uint16]string{0: "Bass", 3: "Drums"}

	SaveStreamNames(dir, names)
	loaded := LoadStreamNames(dir)

	if len(loaded) != 2 {
		t.Fatalf("expected 2, got %d", len(loaded))
	}
	if loaded[0] != "Bass" || loaded[3] != "Drums" {
		t.Fatalf("mismatch: %v", loaded)
	}
}

func TestSaveEmptyOverwrites(t *testing.T) {
	dir := t.TempDir()
	names := map[uint16]string{0: "Bass"}
	SaveStreamNames(dir, names)

	SaveStreamNames(dir, map[uint16]string{})
	loaded := LoadStreamNames(dir)
	if len(loaded) != 0 {
		t.Fatalf("expected empty after overwrite, got %d", len(loaded))
	}
}

func TestLoadInvalidJSON(t *testing.T) {
	dir := t.TempDir()
	os.WriteFile(filepath.Join(dir, streamNamesFilename), []byte("not json"), 0o644)
	names := LoadStreamNames(dir)
	if len(names) != 0 {
		t.Fatal("expected empty for invalid JSON")
	}
}
