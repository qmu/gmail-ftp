package audit

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
)

// Read returns every logged entry in chronological order (oldest first),
// concatenating the rotated segments (.3, .2, .1) ahead of the active log. A
// missing segment is skipped, and a corrupt or partially-written line is dropped
// rather than failing the whole read, so the browser never crashes on a log that
// was being appended to concurrently.
func Read(path string) ([]Entry, error) {
	var entries []Entry
	for i := defaultKeep; i >= 1; i-- { // oldest rotated segments first
		es, err := readSegment(fmt.Sprintf("%s.%d", path, i))
		if err != nil {
			return nil, err
		}
		entries = append(entries, es...)
	}
	es, err := readSegment(path) // active log is newest
	if err != nil {
		return nil, err
	}
	return append(entries, es...), nil
}

func readSegment(path string) ([]Entry, error) {
	f, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	defer f.Close()

	var out []Entry
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 64*1024), 4*1024*1024) // tolerate long lines
	for sc.Scan() {
		line := bytes.TrimSpace(sc.Bytes())
		if len(line) == 0 {
			continue
		}
		var e Entry
		if err := json.Unmarshal(line, &e); err != nil {
			continue // skip a corrupt/partial trailing line
		}
		out = append(out, e)
	}
	return out, sc.Err()
}

// Verb renders an Operation in the CLI's own user-facing vocabulary, matching
// the action words the commands print.
func Verb(op Operation) string {
	switch op {
	case OpDraft:
		return "drafted"
	case OpSend:
		return "sent"
	case OpTrash:
		return "trashed"
	case OpLabel:
		return "labeled"
	case OpUnlabel:
		return "unlabeled"
	case OpMkLabel:
		return "created label"
	default:
		return string(op)
	}
}

// WriteJSON emits entries as a single JSON array (the -json / agent path),
// mirroring the rest of the CLI's JSON conventions. A nil slice emits "[]".
func WriteJSON(w io.Writer, entries []Entry) error {
	if entries == nil {
		entries = []Entry{}
	}
	enc := json.NewEncoder(w)
	enc.SetEscapeHTML(false)
	return enc.Encode(entries)
}

// WriteText emits one human-readable row per entry (the non-TTY plain path),
// newest entries last, matching the order in the file.
func WriteText(w io.Writer, entries []Entry) {
	for _, e := range entries {
		fmt.Fprintf(w, "%s  %-13s  %s  %s\n",
			e.Time.Local().Format("2006-01-02 15:04"), Verb(e.Op), e.Name, e.ID)
	}
}
