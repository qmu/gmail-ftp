package auth

import (
	"encoding/base64"
	"testing"
)

func TestCodeFromRedirect(t *testing.T) {
	const state = "st4te"
	tests := []struct {
		name    string
		input   string
		want    string
		wantErr bool
	}{
		{
			name:  "full redirect URL with matching state",
			input: "http://127.0.0.1:1/?state=st4te&code=4/abc-DEF&scope=gmail.modify",
			want:  "4/abc-DEF",
		},
		{
			name:    "state mismatch is rejected",
			input:   "http://127.0.0.1:1/?state=wrong&code=4/abc",
			wantErr: true,
		},
		{
			name:    "error param surfaces as denial",
			input:   "http://127.0.0.1:1/?error=access_denied&state=st4te",
			wantErr: true,
		},
		{
			name:  "bare code is returned as-is",
			input: "4/abc-DEF_ghi",
			want:  "4/abc-DEF_ghi",
		},
	}
	for _, tt := range tests {
		got, err := codeFromRedirect(tt.input, state)
		if tt.wantErr {
			if err == nil {
				t.Errorf("%s: expected error, got code %q", tt.name, got)
			}
			continue
		}
		if err != nil {
			t.Errorf("%s: unexpected error: %v", tt.name, err)
			continue
		}
		if got != tt.want {
			t.Errorf("%s: codeFromRedirect = %q, want %q", tt.name, got, tt.want)
		}
	}
}

// TestClipboardSeq verifies the OSC 52 framing and base64 payload, plain and
// wrapped for tmux passthrough.
func TestClipboardSeq(t *testing.T) {
	const url = "https://accounts.google.com/o/oauth2/auth?x=1"
	b64 := base64.StdEncoding.EncodeToString([]byte(url))

	t.Run("plain", func(t *testing.T) {
		t.Setenv("TMUX", "")
		want := "\x1b]52;c;" + b64 + "\x07"
		if got := clipboardSeq(url); got != want {
			t.Errorf("clipboardSeq = %q, want %q", got, want)
		}
	})

	t.Run("tmux passthrough", func(t *testing.T) {
		t.Setenv("TMUX", "/tmp/tmux-1000/default,1,0")
		// Expected: DCS prefix + OSC 52 with every ESC doubled + ST terminator.
		want := "\x1bPtmux;\x1b\x1b]52;c;" + b64 + "\x07\x1b\\"
		if got := clipboardSeq(url); got != want {
			t.Errorf("clipboardSeq(tmux) = %q, want %q", got, want)
		}
	})
}

// TestScopesLeastPrivilege guards the locked OAuth scope decision: gmail-ftp must
// request only gmail.modify + gmail.compose and NEVER the full-access
// https://mail.google.com/ scope (which grants permanent delete).
func TestScopesLeastPrivilege(t *testing.T) {
	want := map[string]bool{
		"https://www.googleapis.com/auth/gmail.modify":  false,
		"https://www.googleapis.com/auth/gmail.compose": false,
	}
	for _, s := range Scopes {
		if s == "https://mail.google.com/" {
			t.Fatalf("Scopes must never include the full-access mail.google.com scope")
		}
		if _, ok := want[s]; !ok {
			t.Errorf("unexpected scope %q in Scopes", s)
			continue
		}
		want[s] = true
	}
	for s, seen := range want {
		if !seen {
			t.Errorf("required scope %q missing from Scopes", s)
		}
	}
}
