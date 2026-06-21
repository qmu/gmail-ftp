// Package audit records mutating Gmail operations (draft-create, trash,
// label-create) to an append-only JSON Lines log so a user or AI agent can look
// back at — and recover from — changes the CLI made. The log is owned data: it
// never contains credentials or message contents, only the identity of each
// change. Writes are best-effort; a logging failure must never break the
// operation that triggered it.
package audit

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

// Operation is the kind of Gmail mutation an Entry records. The values mirror
// the CLI's own action vocabulary.
type Operation string

const (
	OpDraft   Operation = "draft"   // a put: a draft created from a local file
	OpSend    Operation = "send"    // a send: a draft sent (the only irreversible op)
	OpTrash   Operation = "trash"   // an rm: a message (or explicit thread) trashed
	OpLabel   Operation = "label"   // a label add on a message (deferred verb)
	OpUnlabel Operation = "unlabel" // a label remove on a message (deferred verb)
	OpMkLabel Operation = "mklabel" // a mkdir: a user label created
)

// Entry is one append-only audit record. It captures what changed — the target
// and, for a label change, the labels involved — never message contents and
// never any credential. Zero-valued optional fields are omitted.
type Entry struct {
	Time     time.Time `json:"time"`
	Op       Operation `json:"op"`
	Name     string    `json:"name"`
	ID       string    `json:"id,omitempty"`
	ThreadID string    `json:"threadId,omitempty"`
	Cwd      string    `json:"cwd,omitempty"`
	Size     int64     `json:"size,omitempty"`
	LabelIDs []string  `json:"labelIds,omitempty"` // labels added/removed for a label/unlabel op
}

// Rotation defaults: the active log is capped, then shifted through a bounded
// ring of segments. 5 MiB × (1 active + 3 rotated) ≈ a 20 MiB ceiling; the
// oldest segment is dropped — the explicit allowable-loss margin.
const (
	defaultMaxSize int64 = 5 << 20 // 5 MiB
	defaultKeep          = 3
)

// Logger appends entries to a JSONL file under the user's config dir, rotating
// it by size. A nil *Logger is a valid no-op logger, so callers can hold one
// unconditionally (audit logging disabled => nil => Record does nothing).
type Logger struct {
	path    string
	maxSize int64
	keep    int
	mu      sync.Mutex
}

// New returns a Logger that appends to path (e.g. ~/.config/gmail-ftp/audit.jsonl).
func New(path string) *Logger {
	return &Logger{path: path, maxSize: defaultMaxSize, keep: defaultKeep}
}

// Record appends e to the log, rotating first if the active file is at capacity.
// A nil Logger and an empty operation are no-ops/errors handled without touching
// disk. The directory is created 0700 and the file 0600, matching the token
// cache's discipline. Record is safe for concurrent use.
func (l *Logger) Record(ctx context.Context, e Entry) error {
	if l == nil {
		return nil
	}
	if e.Op == "" {
		return fmt.Errorf("audit: entry has no operation")
	}
	if e.Time.IsZero() {
		e.Time = time.Now()
	}

	l.mu.Lock()
	defer l.mu.Unlock()

	if dir := filepath.Dir(l.path); dir != "" {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return err
		}
	}
	if err := l.rotateIfNeeded(); err != nil {
		return err
	}

	f, err := os.OpenFile(l.path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer f.Close()

	enc := json.NewEncoder(f)
	enc.SetEscapeHTML(false)
	return enc.Encode(e) // Encode appends a trailing newline -> one JSONL record
}

// rotateIfNeeded rotates the log when the active file has reached the size cap.
// A missing file is not an error (nothing to rotate yet).
func (l *Logger) rotateIfNeeded() error {
	info, err := os.Stat(l.path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return err
	}
	if info.Size() < l.maxSize {
		return nil
	}
	return rotate(l.path, l.keep)
}

// rotate shifts the segment ring: drop path.keep, then path.(keep-1)->path.keep
// … path.1->path.2, and finally path->path.1, leaving the active path free for a
// fresh file. Missing intermediate segments are skipped.
func rotate(path string, keep int) error {
	_ = os.Remove(fmt.Sprintf("%s.%d", path, keep)) // drop the oldest
	for i := keep - 1; i >= 1; i-- {
		from := fmt.Sprintf("%s.%d", path, i)
		if _, err := os.Stat(from); err != nil {
			continue // gap in the ring; nothing to shift here
		}
		if err := os.Rename(from, fmt.Sprintf("%s.%d", path, i+1)); err != nil {
			return err
		}
	}
	return os.Rename(path, path+".1")
}
