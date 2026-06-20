package audit

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestEntryMarshal(t *testing.T) {
	e := Entry{
		Time:     time.Date(2026, 6, 18, 12, 0, 0, 0, time.UTC),
		Op:       OpTrash,
		Name:     "2026-06-18 Quarterly report",
		ID:       "18f1a2b",
		ThreadID: "18f1a2b",
		Cwd:      "/INBOX",
		Size:     840000,
	}
	b, err := json.Marshal(e)
	if err != nil {
		t.Fatal(err)
	}
	got := string(b)
	for _, want := range []string{
		`"time":"2026-06-18T12:00:00Z"`,
		`"op":"trash"`,
		`"name":"2026-06-18 Quarterly report"`,
		`"id":"18f1a2b"`,
		`"threadId":"18f1a2b"`,
		`"cwd":"/INBOX"`,
	} {
		if !strings.Contains(got, want) {
			t.Errorf("marshaled entry missing %s\n  got: %s", want, got)
		}
	}
	// omitempty optional fields absent when zero
	for _, absent := range []string{"labelIds"} {
		if strings.Contains(got, absent) {
			t.Errorf("expected %q to be omitted, got: %s", absent, got)
		}
	}
}

func TestRecordAppendsAndPermissions(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "cfg") // not pre-created: Record must mkdir 0700
	path := filepath.Join(dir, "audit.jsonl")
	l := New(path)
	ctx := context.Background()

	if err := l.Record(ctx, Entry{Op: OpMkLabel, Name: "Work", ID: "Label_1"}); err != nil {
		t.Fatal(err)
	}
	if err := l.Record(ctx, Entry{Op: OpTrash, Name: "old", ID: "18f2"}); err != nil {
		t.Fatal(err)
	}

	// Two JSONL records, each valid JSON, in order.
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	var ops []Operation
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		var e Entry
		if err := json.Unmarshal(sc.Bytes(), &e); err != nil {
			t.Fatalf("line is not valid JSON: %v (%q)", err, sc.Text())
		}
		if e.Time.IsZero() {
			t.Error("Record should stamp a non-zero Time")
		}
		ops = append(ops, e.Op)
	}
	if len(ops) != 2 || ops[0] != OpMkLabel || ops[1] != OpTrash {
		t.Fatalf("ops = %v, want [mklabel trash]", ops)
	}

	// File 0600, dir 0700.
	if fi, _ := os.Stat(path); fi.Mode().Perm() != 0o600 {
		t.Errorf("log file mode = %v, want 0600", fi.Mode().Perm())
	}
	if di, _ := os.Stat(dir); di.Mode().Perm() != 0o700 {
		t.Errorf("log dir mode = %v, want 0700", di.Mode().Perm())
	}
}

func TestRecordNilLoggerIsNoOp(t *testing.T) {
	var l *Logger
	if err := l.Record(context.Background(), Entry{Op: OpMkLabel, Name: "x"}); err != nil {
		t.Errorf("nil logger Record should be a no-op, got %v", err)
	}
}

func TestRecordRejectsEmptyOp(t *testing.T) {
	l := New(filepath.Join(t.TempDir(), "audit.jsonl"))
	if err := l.Record(context.Background(), Entry{Name: "x"}); err == nil {
		t.Error("expected an error for an entry with no operation")
	}
}

func TestRecordRotatesAtCap(t *testing.T) {
	path := filepath.Join(t.TempDir(), "audit.jsonl")
	l := New(path)
	l.maxSize = 80 // tiny cap so a couple of entries trigger rotation
	ctx := context.Background()

	for i := 0; i < 6; i++ {
		if err := l.Record(ctx, Entry{Op: OpMkLabel, Name: "label-with-a-longish-name", ID: "id"}); err != nil {
			t.Fatal(err)
		}
	}
	// Rotation must have produced at least audit.jsonl.1, and never exceed .keep segments.
	if _, err := os.Stat(path + ".1"); err != nil {
		t.Errorf("expected a rotated segment audit.jsonl.1: %v", err)
	}
	if _, err := os.Stat(path + ".4"); !os.IsNotExist(err) {
		t.Errorf("segments beyond keep=%d must not exist; .4 present", l.keep)
	}
}

func TestRotateShiftsAndDropsOldest(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "audit.jsonl")
	// Seed active + a full ring, each with identifiable content.
	write := func(p, content string) {
		if err := os.WriteFile(p, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write(path, "active")
	write(path+".1", "seg1")
	write(path+".2", "seg2")
	write(path+".3", "seg3") // oldest; should be dropped

	if err := rotate(path, 3); err != nil {
		t.Fatal(err)
	}

	// active->.1, .1->.2, .2->.3, old .3 dropped, active now gone.
	checks := map[string]string{path + ".1": "active", path + ".2": "seg1", path + ".3": "seg2"}
	for p, want := range checks {
		got, err := os.ReadFile(p)
		if err != nil || string(got) != want {
			t.Errorf("%s = %q (err %v), want %q", filepath.Base(p), got, err, want)
		}
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Errorf("active path should be free after rotate, still exists")
	}
}
