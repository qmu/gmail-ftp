package audit

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestReadConcatenatesSegmentsOldestFirst(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "audit.jsonl")
	ctx := context.Background()

	// Write distinct names into rotated segments and the active log. Segment .2
	// is the oldest present (a gap at .3 must be tolerated).
	writeSeg := func(p, name string) {
		l := New(p)
		if err := l.Record(ctx, Entry{Op: OpMkLabel, Name: name}); err != nil {
			t.Fatal(err)
		}
	}
	writeSeg(path+".2", "oldest")
	writeSeg(path+".1", "middle")
	writeSeg(path, "newest")

	entries, err := Read(path)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, e := range entries {
		names = append(names, e.Name)
	}
	want := []string{"oldest", "middle", "newest"}
	if strings.Join(names, ",") != strings.Join(want, ",") {
		t.Errorf("Read order = %v, want %v", names, want)
	}
}

func TestReadSkipsCorruptLineAndMissingFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "audit.jsonl")
	// One valid line, one corrupt/partial line (e.g. a truncated concurrent write).
	content := `{"time":"2026-06-18T12:00:00Z","op":"trash","name":"a","id":"1"}` + "\n" +
		`{"time":"2026-06-18T12:01:00Z","op":"draf` // truncated
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	entries, err := Read(path) // no rotated segments exist -> they're skipped
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name != "a" {
		t.Fatalf("expected only the valid entry, got %+v", entries)
	}
}

func TestVerb(t *testing.T) {
	cases := map[Operation]string{
		OpDraft:        "drafted",
		OpSend:         "sent",
		OpTrash:        "trashed",
		OpLabel:        "labeled",
		OpUnlabel:      "unlabeled",
		OpMkLabel:      "created label",
		Operation("x"): "x",
	}
	for op, want := range cases {
		if got := Verb(op); got != want {
			t.Errorf("Verb(%q) = %q, want %q", op, got, want)
		}
	}
}

func TestWriteJSONEmptyIsArray(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteJSON(&buf, nil); err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimSpace(buf.String()); got != "[]" {
		t.Errorf("WriteJSON(nil) = %q, want []", got)
	}
}
