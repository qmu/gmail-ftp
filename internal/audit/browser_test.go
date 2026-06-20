package audit

import (
	"strings"
	"testing"
	"time"
)

func TestMoveCursorClamps(t *testing.T) {
	tests := []struct {
		cursor, delta, n, want int
	}{
		{0, -1, 5, 0},  // clamp at top
		{4, 1, 5, 4},   // clamp at bottom
		{2, 1, 5, 3},   // normal down
		{2, -1, 5, 1},  // normal up
		{0, 1, 0, 0},   // empty list
		{3, -10, 5, 0}, // big jump up clamps
		{1, 10, 5, 4},  // big jump down clamps
	}
	for _, tt := range tests {
		if got := moveCursor(tt.cursor, tt.delta, tt.n); got != tt.want {
			t.Errorf("moveCursor(%d,%d,%d) = %d, want %d", tt.cursor, tt.delta, tt.n, got, tt.want)
		}
	}
}

func TestViewportTop(t *testing.T) {
	tests := []struct {
		name                         string
		cursor, top, height, n, want int
	}{
		{"cursor within window keeps top", 3, 2, 5, 20, 2},
		{"cursor above window scrolls up", 1, 4, 5, 20, 1},
		{"cursor below window scrolls down", 9, 2, 5, 20, 5},
		{"clamp top so last page is full", 19, 0, 5, 20, 15},
		{"empty list", 0, 0, 5, 0, 0},
		{"height larger than n", 2, 0, 50, 3, 0},
	}
	for _, tt := range tests {
		if got := viewportTop(tt.cursor, tt.top, tt.height, tt.n); got != tt.want {
			t.Errorf("%s: viewportTop(%d,%d,%d,%d) = %d, want %d",
				tt.name, tt.cursor, tt.top, tt.height, tt.n, got, tt.want)
		}
	}
}

func TestRowTextTruncates(t *testing.T) {
	e := Entry{Time: time.Date(2026, 6, 18, 17, 41, 0, 0, time.UTC), Op: OpTrash, Name: strings.Repeat("x", 200), ID: "abc"}
	got := rowText(e, 40)
	if len([]rune(got)) > 40 {
		t.Errorf("rowText width = %d runes, want <= 40", len([]rune(got)))
	}
	if !strings.HasSuffix(got, "…") {
		t.Errorf("truncated row should end with an ellipsis, got %q", got)
	}
	// Untruncated when it fits.
	short := rowText(Entry{Time: e.Time, Op: OpTrash, Name: "a", ID: "1"}, 120)
	if strings.Contains(short, "…") {
		t.Errorf("short row should not be truncated, got %q", short)
	}
}
