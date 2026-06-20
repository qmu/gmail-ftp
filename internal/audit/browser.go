package audit

import (
	"fmt"
	"os"
	"strings"

	"golang.org/x/term"
)

// Browse opens a read-only, tig-like terminal browser over entries (newest
// first): j/↓ and k/↑ move the cursor, g/G jump to top/bottom, Enter shows the
// selected entry's detail, and q/Esc/Ctrl-C quit. It is built on the standard
// library plus golang.org/x/term (already vendored) — no new dependencies — and
// never mutates anything. Callers should handle the empty and non-TTY cases
// before calling Browse.
func Browse(entries []Entry) error {
	// Present newest-first without mutating the caller's slice.
	rev := make([]Entry, len(entries))
	for i, e := range entries {
		rev[len(entries)-1-i] = e
	}

	fd := int(os.Stdin.Fd())
	old, err := term.MakeRaw(fd)
	if err != nil {
		// Degrade gracefully to a plain dump, like the shell falls back to a scanner.
		WriteText(os.Stdout, entries)
		return nil
	}
	defer term.Restore(fd, old)
	out := os.Stdout
	fmt.Fprint(out, "\x1b[?25l")       // hide cursor
	defer fmt.Fprint(out, "\x1b[?25h") // show cursor on exit

	cursor, top := 0, 0
	var buf [3]byte
	for {
		width, height := termSize(fd)
		rows := height - 2 // header + footer
		if rows < 1 {
			rows = 1
		}
		top = viewportTop(cursor, top, rows, len(rev))
		fmt.Fprint(out, render(rev, cursor, top, rows, width))

		n, err := os.Stdin.Read(buf[:])
		if err != nil || n == 0 {
			return nil
		}
		// Arrow keys arrive as a 3-byte ESC [ A/B burst in a single read.
		if n == 3 && buf[0] == 0x1b && buf[1] == '[' {
			switch buf[2] {
			case 'A':
				cursor = moveCursor(cursor, -1, len(rev))
			case 'B':
				cursor = moveCursor(cursor, 1, len(rev))
			}
			continue
		}
		switch buf[0] {
		case 'q', 0x1b, 0x03: // q, Esc, Ctrl-C
			fmt.Fprint(out, "\x1b[2J\x1b[H")
			return nil
		case 'j':
			cursor = moveCursor(cursor, 1, len(rev))
		case 'k':
			cursor = moveCursor(cursor, -1, len(rev))
		case 'g':
			cursor = 0
		case 'G':
			cursor = len(rev) - 1
		case '\r', '\n': // Enter -> detail, then return to the list
			showDetail(out, rev[cursor], fd)
		}
	}
}

// moveCursor returns cursor+delta clamped to [0, n-1] (0 when n == 0).
func moveCursor(cursor, delta, n int) int {
	if n == 0 {
		return 0
	}
	c := cursor + delta
	if c < 0 {
		return 0
	}
	if c > n-1 {
		return n - 1
	}
	return c
}

// viewportTop returns the list's top index so that cursor stays visible within a
// window of height rows over n items.
func viewportTop(cursor, top, height, n int) int {
	if height <= 0 || n == 0 {
		return 0
	}
	if cursor < top {
		top = cursor
	} else if cursor >= top+height {
		top = cursor - height + 1
	}
	if max := n - height; top > max {
		top = max
	}
	if top < 0 {
		top = 0
	}
	return top
}

// render builds the full screen frame: a header, the visible rows (with the
// selected row in reverse video), and a key-hint footer.
func render(entries []Entry, cursor, top, rows, width int) string {
	var b strings.Builder
	b.WriteString("\x1b[2J\x1b[H") // clear + home
	fmt.Fprintf(&b, " gmail-ftp audit log — %d operation(s)\r\n", len(entries))
	for i := top; i < top+rows && i < len(entries); i++ {
		line := rowText(entries[i], width)
		if i == cursor {
			fmt.Fprintf(&b, "\x1b[7m%s\x1b[0m\r\n", line)
		} else {
			fmt.Fprintf(&b, "%s\r\n", line)
		}
	}
	b.WriteString(" j/k move · g/G top/bottom · enter detail · q quit")
	return b.String()
}

// rowText formats one entry as a single line, truncated to width.
func rowText(e Entry, width int) string {
	line := fmt.Sprintf(" %s  %-13s  %s  %s",
		e.Time.Local().Format("2006-01-02 15:04"), Verb(e.Op), e.Name, e.ID)
	if width > 1 && len(line) > width {
		line = line[:width-1] + "…"
	}
	return line
}

// showDetail renders the full record of e and waits for any key to return.
func showDetail(out *os.File, e Entry, fd int) {
	var b strings.Builder
	b.WriteString("\x1b[2J\x1b[H")
	fmt.Fprintf(&b, " %s — %s\r\n\r\n", Verb(e.Op), e.Name)
	add := func(label, val string) {
		if val != "" {
			fmt.Fprintf(&b, "  %-12s %s\r\n", label+":", val)
		}
	}
	add("time", e.Time.Local().Format("2006-01-02 15:04:05"))
	add("id", e.ID)
	add("thread", e.ThreadID)
	add("cwd", e.Cwd)
	if e.Size > 0 {
		add("size", fmt.Sprintf("%d", e.Size))
	}
	if len(e.LabelIDs) > 0 {
		add("labels", strings.Join(e.LabelIDs, ", "))
	}
	b.WriteString("\r\n press any key to return")
	fmt.Fprint(out, b.String())

	var k [1]byte
	_, _ = os.Stdin.Read(k[:])
}

// termSize returns the terminal width and height, falling back to a sane default
// when the size cannot be determined (e.g. a pipe).
func termSize(fd int) (int, int) {
	w, h, err := term.GetSize(fd)
	if err != nil || w <= 0 || h <= 0 {
		return 80, 24
	}
	return w, h
}
