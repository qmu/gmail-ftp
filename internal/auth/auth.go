// Package auth handles the Google OAuth2 flow for the Gmail API and caches the
// resulting token on disk so the flow only runs once.
package auth

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"

	"golang.org/x/oauth2"
	"golang.org/x/oauth2/google"
	"golang.org/x/term"
	gmail "google.golang.org/api/gmail/v1"
)

// Scopes is the least-privilege set of OAuth scopes gmail-ftp requests. It is the
// single source of truth for the program's Gmail permissions (README and SKILL.md
// quote it):
//
//   - gmail.modify covers every read path (ls/cd/get/find/search), trashing a
//     message (rm), label create (mkdir), and message label mutation. It
//     deliberately subsumes read access and CANNOT permanently delete mail — the
//     same "trash, not hard-delete" safety property gdrive-ftp relies on. The full
//     https://mail.google.com/ scope (which grants hard-delete) is never requested.
//   - gmail.compose covers creating a draft (put) and, in v1.1, sending.
//
// Sending is gated behind the explicit (deferred) send verb; v1 never sends.
var Scopes = []string{
	gmail.GmailModifyScope,  // read + trash + label-create + label-modify (no hard-delete)
	gmail.GmailComposeScope, // create drafts (put); send is deferred to v1.1
}

// Client builds an authorized *http.Client for the Gmail API. On first use it
// runs the OAuth consent flow and caches the token at tokenPath; subsequent
// runs reuse (and silently refresh) that token.
//
// credsPath points at an OAuth "Desktop app" client_credentials.json file
// downloaded from the Google Cloud Console. Consent is done entirely over the
// terminal (copy the URL out, paste the redirect URL back), so it works the
// same on a local machine or a headless/SSH host.
func Client(ctx context.Context, credsPath, tokenPath string) (*http.Client, error) {
	b, err := os.ReadFile(credsPath)
	if err != nil {
		return nil, fmt.Errorf("reading credentials %q: %w\n"+
			"Download an OAuth \"Desktop app\" client from the Google Cloud Console "+
			"(APIs & Services > Credentials) and save it there, or pass -creds.", credsPath, err)
	}
	// The least-privilege Gmail scopes cover read, trash, label, and draft;
	// they never grant permanent delete (see Scopes).
	config, err := google.ConfigFromJSON(b, Scopes...)
	if err != nil {
		return nil, fmt.Errorf("parsing credentials: %w", err)
	}

	tok, err := tokenFromFile(tokenPath)
	if err != nil {
		tok, err = tokenFromWeb(ctx, config)
		if err != nil {
			return nil, err
		}
		if err := saveToken(tokenPath, tok); err != nil {
			return nil, fmt.Errorf("caching token: %w", err)
		}
		fmt.Fprintf(os.Stderr, "Saved authorization token to %s\n", tokenPath)
	}

	// Wrap the refreshing source so rotated tokens are written back to disk.
	src := &savingSource{base: config.TokenSource(ctx, tok), path: tokenPath, last: tok}
	return oauth2.NewClient(ctx, src), nil
}

// tokenFromWeb walks the user through the OAuth consent screen and returns the
// resulting token.
func tokenFromWeb(ctx context.Context, config *oauth2.Config) (*oauth2.Token, error) {
	state, err := randomState()
	if err != nil {
		return nil, err
	}
	return consentFlow(ctx, config, state)
}

// consentFlow runs the terminal OAuth consent flow used everywhere (local or
// SSH). It prints the consent URL and offers a single-keypress prompt: 'c'
// copies the URL to the user's local clipboard via the OSC 52 terminal escape
// (the only channel that reaches a laptop over SSH), 'o' attempts to open a
// local browser, and any other key falls through to manual copy. After
// authorizing, the user pastes the entire http://127.0.0.1:1/...?state=...&code=...
// redirect URL their browser lands on; we extract the code and validate state.
func consentFlow(ctx context.Context, config *oauth2.Config, state string) (*oauth2.Token, error) {
	config.RedirectURL = "http://127.0.0.1:1/"
	authURL := config.AuthCodeURL(state, oauth2.AccessTypeOffline, oauth2.ApprovalForce)

	fmt.Fprintf(os.Stderr,
		"To authorize gmail-ftp, open this URL in your local browser:\n\n%s\n\n", authURL)
	switch promptKey("Press 'c' to copy the URL to your local clipboard, 'o' to open it in a browser here, or any other key to copy it yourself: ") {
	case 'c':
		copyToClipboard(authURL)
		fmt.Fprintln(os.Stderr, "Copied to clipboard.")
	case 'o':
		openBrowser(authURL)
	}

	fmt.Fprint(os.Stderr,
		"\nAfter you authorize, the browser redirects to a http://127.0.0.1:1/...\n"+
			"URL that fails to load. Paste that entire URL here (or just the code): ")
	line, err := readLine()
	if err != nil {
		return nil, fmt.Errorf("reading redirect URL: %w", err)
	}
	code, err := codeFromRedirect(line, state)
	if err != nil {
		return nil, err
	}
	return config.Exchange(ctx, code)
}

// promptKey prints prompt and reads one keypress without waiting for Enter,
// returning it lowercased. When stdin is not a terminal (e.g. piped input) it
// falls back to reading a whole line and using its first character.
func promptKey(prompt string) byte {
	fmt.Fprint(os.Stderr, prompt)
	fd := int(os.Stdin.Fd())
	if !term.IsTerminal(fd) {
		line, _ := readLine()
		if line == "" {
			fmt.Fprintln(os.Stderr)
			return 0
		}
		return lowerASCII(line[0])
	}
	old, err := term.MakeRaw(fd)
	if err != nil {
		line, _ := readLine()
		if line == "" {
			return 0
		}
		return lowerASCII(line[0])
	}
	var b [1]byte
	n, _ := os.Stdin.Read(b[:])
	_ = term.Restore(fd, old)
	if n == 0 {
		fmt.Fprintln(os.Stderr)
		return 0
	}
	fmt.Fprintf(os.Stderr, "%c\n", b[0]) // echo the key (raw mode suppresses it)
	return lowerASCII(b[0])
}

// readLine reads a single non-blank line from stdin, skipping stray blank lines
// (e.g. a leftover newline after the single-key prompt).
func readLine() (string, error) {
	r := bufio.NewReader(os.Stdin)
	for {
		l, err := r.ReadString('\n')
		if s := strings.TrimSpace(l); s != "" {
			return s, nil
		}
		if err != nil {
			return "", err
		}
	}
}

// lowerASCII lowercases an ASCII byte.
func lowerASCII(b byte) byte {
	if b >= 'A' && b <= 'Z' {
		return b + ('a' - 'A')
	}
	return b
}

// codeFromRedirect extracts the OAuth authorization code from a pasted redirect
// URL, validating the embedded state against the expected CSRF value and
// surfacing an error= denial. If the input does not parse as a URL carrying a
// code (the user pasted the bare code), the trimmed input is returned as-is.
func codeFromRedirect(input, state string) (string, error) {
	if u, err := url.Parse(input); err == nil {
		q := u.Query()
		if e := q.Get("error"); e != "" {
			return "", fmt.Errorf("authorization denied: %s", e)
		}
		if code := q.Get("code"); code != "" {
			if q.Get("state") != state {
				return "", fmt.Errorf("state mismatch (possible CSRF); aborting")
			}
			return code, nil
		}
	}
	// Not a redirect URL carrying a code or error; treat input as the bare code.
	return input, nil
}

// copyToClipboard puts s on the user's local clipboard via the OSC 52 terminal
// escape, which a terminal emulator honors even across SSH — so we deliberately
// do not shell out to xclip/pbcopy (those target the remote host). Inside tmux
// the sequence is wrapped in the DCS passthrough so it reaches the outer
// terminal instead of being swallowed; it is written to the controlling
// terminal (/dev/tty) so it works even when stderr is redirected.
func copyToClipboard(s string) {
	seq := clipboardSeq(s)
	if tty, err := os.OpenFile("/dev/tty", os.O_WRONLY, 0); err == nil {
		fmt.Fprint(tty, seq)
		tty.Close()
		return
	}
	fmt.Fprint(os.Stderr, seq)
}

// clipboardSeq builds the OSC 52 escape that sets the clipboard to s. Inside
// tmux it is wrapped in the DCS passthrough (DCS prefix, every ESC doubled, ST
// terminator) so the escape reaches the outer terminal instead of being
// swallowed by tmux.
func clipboardSeq(s string) string {
	seq := "\x1b]52;c;" + base64.StdEncoding.EncodeToString([]byte(s)) + "\x07"
	if os.Getenv("TMUX") != "" {
		seq = "\x1bPtmux;" + strings.ReplaceAll(seq, "\x1b", "\x1b\x1b") + "\x1b\\"
	}
	return seq
}

// savingSource is an oauth2.TokenSource that persists the token whenever the
// underlying source rotates it (e.g. an access-token refresh).
type savingSource struct {
	base oauth2.TokenSource
	path string
	mu   sync.Mutex
	last *oauth2.Token
}

func (s *savingSource) Token() (*oauth2.Token, error) {
	t, err := s.base.Token()
	if err != nil {
		return nil, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.last == nil || t.AccessToken != s.last.AccessToken || !t.Expiry.Equal(s.last.Expiry) {
		_ = saveToken(s.path, t) // best effort; an unwritable cache must not break the session
		s.last = t
	}
	return t, nil
}

func tokenFromFile(path string) (*oauth2.Token, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	tok := &oauth2.Token{}
	if err := json.NewDecoder(f).Decode(tok); err != nil {
		return nil, err
	}
	return tok, nil
}

func saveToken(path string, tok *oauth2.Token) error {
	if dir := filepath.Dir(path); dir != "" {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return err
		}
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	defer f.Close()
	enc := json.NewEncoder(f)
	enc.SetIndent("", "  ")
	return enc.Encode(tok)
}

func randomState() (string, error) {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}

// openBrowser best-effort opens url in the user's default browser. Failure is
// silently ignored because the URL is also printed for manual use.
func openBrowser(url string) {
	var cmd string
	var args []string
	switch runtime.GOOS {
	case "darwin":
		cmd = "open"
	case "windows":
		cmd, args = "rundll32", []string{"url.dll,FileProtocolHandler"}
	default:
		cmd = "xdg-open"
	}
	args = append(args, url)
	_ = exec.Command(cmd, args...).Start()
}
