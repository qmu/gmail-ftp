// Package shell implements the interactive, FTP-like command loop over a Gmail
// client: ls, cd, pwd, find, get, put (draft), rm (trash), mkdir (label) and
// local-side helpers. Navigation is two levels — root → label → message — with
// attachments as leaves inside a message; threads are opt-in via id:thread:<id>,
// never a navigable tier.
package shell

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"regexp"
	"sort"
	"strings"

	"gmail-ftp/internal/audit"
	gmailpkg "gmail-ftp/internal/gmail"

	"golang.org/x/term"
	gmail "google.golang.org/api/gmail/v1"
	"google.golang.org/api/googleapi"
)

// gmailClient is the narrow backend surface the shell depends on. Defining it
// here (rather than depending on the concrete *gmail.Client) keeps command
// dispatch and output unit-testable with a fake — no live Gmail credentials.
// *gmail.Client satisfies it.
type gmailClient interface {
	ListLabels(ctx context.Context) ([]gmailpkg.Label, error)
	ListMessages(ctx context.Context, labelID, query string) (*gmailpkg.MessageList, error)
	Search(ctx context.Context, query string) (*gmailpkg.MessageList, error)
	GetMessage(ctx context.Context, id, format string) (*gmail.Message, error)
	GetRawMessage(ctx context.Context, id string) ([]byte, error)
	GetAttachment(ctx context.Context, msgID, attID string) ([]byte, error)
	GetThread(ctx context.Context, id string) (*gmail.Thread, error)
	CreateDraft(ctx context.Context, raw []byte) (*gmail.Draft, error)
	GetDraftRaw(ctx context.Context, draftID string) (threadID string, raw []byte, err error)
	UpdateDraft(ctx context.Context, draftID string, raw []byte) (*gmail.Draft, error)
	SendDraft(ctx context.Context, draftID string) (*gmail.Message, error)
	TrashMessage(ctx context.Context, id string) (*gmail.Message, error)
	TrashThread(ctx context.Context, id string) (*gmail.Thread, error)
	ModifyLabels(ctx context.Context, id string, add, remove []string) (*gmail.Message, error)
	CreateLabel(ctx context.Context, name string) (*gmail.Label, error)
	FindLabel(ctx context.Context, name string) (gmailpkg.Label, error)
	FindMessageByName(ctx context.Context, labelID, name string) (*gmail.Message, error)
}

// Shell holds the session state: the Gmail client and the current remote working
// directory. The cwd is the chain of path elements from the virtual root; an
// empty cwd means the virtual root, whose entries are the labels. In the 2-level
// model the cwd is at most one element deep — a label — because a message is a
// leaf you cannot cd into.
type Shell struct {
	ctx     context.Context
	c       gmailClient
	cwd     []gmailpkg.Ref // path from the virtual root; empty means the virtual root
	out     io.Writer
	jsonOut bool           // emit machine-readable JSON instead of human text
	log     *audit.Logger  // append-only audit log of mutations; nil disables logging
	term    *term.Terminal // set only while the interactive line editor is active
}

// New creates a Shell positioned at the virtual root, which lists the user's
// labels. When jsonOut is true, commands emit machine-readable JSON instead of
// human-formatted text. log records mutating operations (a nil log disables audit
// logging).
func New(ctx context.Context, c gmailClient, out io.Writer, jsonOut bool, log *audit.Logger) *Shell {
	return &Shell{ctx: ctx, c: c, out: out, jsonOut: jsonOut, log: log}
}

// audit records a mutation to the audit log. It is best-effort: a write failure
// never breaks the command (the mutation already happened) and is surfaced only
// as a one-line warning on stderr, keeping stdout output clean (e.g. for -json).
func (s *Shell) audit(e audit.Entry) {
	if err := s.log.Record(s.ctx, e); err != nil {
		fmt.Fprintf(os.Stderr, "gmail-ftp: audit log write failed: %v\n", err)
	}
}

// command is a single REPL verb.
type command struct {
	run   func(s *Shell, args []string) error
	usage string
	help  string
}

// commands is the dispatch table, populated in commands.go.
var commands map[string]command

// Run reads commands until EOF (Ctrl-D) or a quit verb. When interactive and
// stdin is a terminal it uses a line editor with Tab completion; otherwise it
// falls back to a plain line scanner (pipes, one-shot, non-TTY).
func (s *Shell) Run(interactive bool) error {
	if interactive && term.IsTerminal(int(os.Stdin.Fd())) {
		return s.runTerminal()
	}
	return s.runScanner(interactive)
}

// runScanner is the plain, non-interactive read loop (no line editing).
func (s *Shell) runScanner(interactive bool) error {
	sc := bufio.NewScanner(os.Stdin)
	sc.Buffer(make([]byte, 64*1024), 4*1024*1024)
	for {
		if interactive {
			fmt.Fprintf(s.out, "gmail:%s> ", s.pwd())
		}
		if !sc.Scan() {
			if interactive {
				fmt.Fprintln(s.out)
			}
			break
		}
		args, err := tokenize(sc.Text())
		if err != nil {
			fmt.Fprintln(s.out, "parse error:", err)
			continue
		}
		if len(args) == 0 {
			continue
		}
		if quit := s.dispatch(args); quit {
			break
		}
	}
	return sc.Err()
}

// runTerminal drives the interactive shell through a raw-mode line editor
// (golang.org/x/term) so the user gets line editing and Tab completion. It
// degrades to runScanner if raw mode cannot be entered. While active, command
// output is routed through a CRLF-translating writer so plain "\n" lines render
// correctly in raw mode.
func (s *Shell) runTerminal() error {
	fd := int(os.Stdin.Fd())
	old, err := term.MakeRaw(fd)
	if err != nil {
		return s.runScanner(true)
	}
	defer term.Restore(fd, old)

	rw := struct {
		io.Reader
		io.Writer
	}{os.Stdin, os.Stdout}
	t := term.NewTerminal(rw, "")
	t.AutoCompleteCallback = s.autoComplete
	s.term = t
	prevOut := s.out
	s.out = crlfWriter{os.Stdout}
	defer func() { s.term = nil; s.out = prevOut }()

	for {
		t.SetPrompt(fmt.Sprintf("gmail:%s> ", s.pwd()))
		line, err := t.ReadLine()
		if errors.Is(err, io.EOF) {
			fmt.Fprint(s.out, "\n")
			return nil
		}
		if err != nil {
			return err
		}
		args, perr := tokenize(line)
		if perr != nil {
			fmt.Fprintln(s.out, "parse error:", perr)
			continue
		}
		if len(args) == 0 {
			continue
		}
		if quit := s.dispatch(args); quit {
			return nil
		}
	}
}

// crlfWriter translates a lone "\n" into "\r\n" so command output prints
// correctly while the terminal is in raw mode.
type crlfWriter struct{ w io.Writer }

func (c crlfWriter) Write(p []byte) (int, error) {
	out := make([]byte, 0, len(p)+8)
	for _, b := range p {
		if b == '\n' {
			out = append(out, '\r', '\n')
		} else {
			out = append(out, b)
		}
	}
	if _, err := c.w.Write(out); err != nil {
		return 0, err
	}
	return len(p), nil
}

// Execute runs a single command (non-interactive one-shot mode).
func (s *Shell) Execute(args []string) error {
	if len(args) == 0 {
		return nil
	}
	if name := args[0]; name == "quit" || name == "exit" || name == "bye" {
		return nil
	}
	cmd, ok := commands[args[0]]
	if !ok {
		return fmt.Errorf("unknown command %q (try 'help')", args[0])
	}
	return friendlyErr(cmd.run(s, args[1:]))
}

// friendlyErr rewrites well-known Gmail API misconfigurations into an actionable
// message instead of surfacing the raw googleapi JSON dump. Unknown errors pass
// through unchanged.
func friendlyErr(err error) error {
	if err == nil {
		return nil
	}
	var ge *googleapi.Error
	if errors.As(err, &ge) && ge.Code == 403 {
		disabled := strings.Contains(ge.Message, "has not been used in project") ||
			strings.Contains(err.Error(), "SERVICE_DISABLED")
		for _, e := range ge.Errors {
			if e.Reason == "accessNotConfigured" {
				disabled = true
			}
		}
		if disabled {
			enableURL := "https://console.cloud.google.com/apis/library/gmail.googleapis.com"
			project := "<your-project-id>"
			// Prefer the exact, project-specific activation URL Google returned.
			if u := activationURL(err.Error()); u != "" {
				enableURL = u
			}
			if p := projectNumber(err.Error()); p != "" {
				project = p
			}
			return fmt.Errorf("the Gmail API is disabled for this OAuth client's Google Cloud project.\n"+
				"Enable it, wait ~1 minute for it to propagate, then retry:\n"+
				"  • Console: %s\n"+
				"  • or:      gcloud services enable gmail.googleapis.com --project=%s", enableURL, project)
		}
	}
	return err
}

var (
	reActivationURL = regexp.MustCompile(`https://[^\s"]*gmail\.googleapis\.com[^\s"]*overview\?project=\d+`)
	reProjectNumber = regexp.MustCompile(`project[s/=]+(\d+)`)
)

// activationURL extracts Google's exact "enable this API" console URL from an
// error message, or "" if none is present.
func activationURL(msg string) string {
	return reActivationURL.FindString(msg)
}

// projectNumber extracts the GCP project number from an error message, or "".
func projectNumber(msg string) string {
	if m := reProjectNumber.FindStringSubmatch(msg); m != nil {
		return m[1]
	}
	return ""
}

// --- Tab completion ---

// autoComplete is the term.Terminal callback. It acts only on Tab: it completes
// the active token (command verb, remote path, or local path) and, when several
// candidates remain, prints them above the prompt like sftp.
func (s *Shell) autoComplete(line string, pos int, key rune) (string, int, bool) {
	if key != '\t' {
		return "", 0, false
	}
	left := line[:pos]
	newLeft, candidates := s.completeInput(left)
	if len(candidates) > 1 && s.term != nil {
		fmt.Fprintf(s.term, "\r\n%s\r\n", strings.Join(candidates, "  "))
	}
	if newLeft == left {
		return "", 0, false
	}
	return newLeft + line[pos:], len(newLeft), true
}

// completeInput computes the completion for the text left of the cursor. It
// returns the rewritten left text (unchanged if nothing to complete) and, when
// the result is ambiguous, the list of candidate names to display.
func (s *Shell) completeInput(left string) (string, []string) {
	toks, err := tokenize(left)
	if err != nil { // e.g. an unterminated quote — don't guess
		return left, nil
	}
	endsSpace := left == "" || strings.HasSuffix(left, " ") || strings.HasSuffix(left, "\t")
	idx, active := len(toks), ""
	if !endsSpace && len(toks) > 0 {
		idx, active = len(toks)-1, toks[idx-1]
	}

	// Gather candidate names for this position.
	var names []string
	if idx == 0 {
		names = completionVerbs()
	} else {
		dir, _ := splitPath(active)
		switch argKind(toks[0], idx) {
		case "remote":
			names = s.remoteNames(dir)
		case "local":
			names = s.localNames(dir)
		default:
			return left, nil
		}
	}

	base := active
	if idx > 0 {
		_, base = splitPath(active)
	}
	matches := filterByPrefix(names, base)
	if len(matches) == 0 {
		return left, nil
	}

	// Build the completed token: keep the directory prefix, replace the base.
	completedBase := longestCommonPrefix(matches)
	pathPart := completedBase
	if idx > 0 {
		dir, _ := splitPath(active)
		pathPart = dir + completedBase
	}
	rendered := quoteArg(pathPart)
	// A single, fully-resolved leaf gets a trailing space; a label (ends in "/")
	// does not, so the user can Tab straight into it.
	if len(matches) == 1 && !strings.HasSuffix(matches[0], "/") {
		rendered += " "
	}

	newLeft := left[:lastTokenStart(left)] + rendered
	if len(matches) > 1 {
		return newLeft, matches
	}
	return newLeft, nil
}

// Complete returns shell-completion candidates for a command line given as the
// already-split words after the program name, where the final word is the one
// being completed (possibly empty). It powers external shell completion (e.g.
// the zsh script) and reuses the same candidate logic as interactive Tab. Each
// returned candidate is the full word value (directory prefix included), with
// labels suffixed by "/". Errors yield no candidates.
func (s *Shell) Complete(words []string) []string {
	if len(words) <= 1 {
		prefix := ""
		if len(words) == 1 {
			prefix = words[0]
		}
		return filterByPrefix(completionVerbs(), prefix)
	}
	verb := words[0]
	argIndex := len(words) - 1
	cur := words[argIndex]
	dir, base := splitPath(cur)
	var names []string
	switch argKind(verb, argIndex) {
	case "remote":
		names = s.remoteNames(dir)
	case "local":
		names = s.localNames(dir)
	default:
		return nil
	}
	matches := filterByPrefix(names, base)
	out := make([]string, 0, len(matches))
	for _, m := range matches {
		out = append(out, dir+m)
	}
	return out
}

// completionVerbs returns the command verbs offered for first-token completion.
func completionVerbs() []string {
	names := append(sortedCommandNames(), "quit", "exit", "bye")
	sort.Strings(names)
	return names
}

// argKind reports whether argument argIndex of verb names a remote path, a local
// path, or neither (no completion).
func argKind(verb string, argIndex int) string {
	switch verb {
	case "ls", "cd", "rm", "mkdir":
		if argIndex == 1 {
			return "remote"
		}
	case "find", "search":
		// arg 1 is the query/pattern (no completion); arg 2 is the start label.
		if argIndex == 2 {
			return "remote"
		}
	case "get":
		switch argIndex {
		case 1:
			return "remote"
		case 2:
			return "local"
		}
	case "put":
		// arg 1 is the local file; arg 2 (attach target) is a draft id, not a
		// completable remote path.
		if argIndex == 1 {
			return "local"
		}
	case "compose":
		// compose takes flags and an optional local body-file; the body-file is
		// the only completable positional and only when it is not a flag value.
		if argIndex >= 1 {
			return "local"
		}
	case "label", "unlabel":
		// arg 1 is the target message (remote); arg 2 is a label name (no completion).
		if argIndex == 1 {
			return "remote"
		}
	case "send":
		// arg 1 is a draft id (id:<draftId> / id:draft:<id>), not a completable path.
		return ""
	case "lcd", "lls":
		if argIndex == 1 {
			return "local"
		}
	}
	return ""
}

// remoteNames lists the entries of remote directory dir (relative to the cwd or
// absolute) as completion candidates, labels suffixed with "/". At the virtual
// root it returns the label names; inside a label it returns synthesized message
// names. Any error yields no candidates.
func (s *Shell) remoteNames(dir string) []string {
	stack, err := s.resolveDir(dir)
	if err != nil {
		return nil
	}
	if len(stack) == 0 {
		labels, err := s.c.ListLabels(s.ctx)
		if err != nil {
			return nil
		}
		names := make([]string, 0, len(labels))
		for _, l := range labels {
			names = append(names, l.Name+"/")
		}
		return names
	}
	list, err := s.c.ListMessages(s.ctx, currentID(stack), "")
	if err != nil {
		return nil
	}
	names := make([]string, 0, len(list.Messages))
	for _, m := range list.Messages {
		names = append(names, gmailpkg.MessageName(m))
	}
	return names
}

// localNames lists the entries of local directory dir as completion candidates,
// directories suffixed with "/". Any error yields no candidates.
func (s *Shell) localNames(dir string) []string {
	if dir == "" {
		dir = "."
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil
	}
	names := make([]string, 0, len(entries))
	for _, e := range entries {
		n := e.Name()
		if e.IsDir() {
			n += "/"
		}
		names = append(names, n)
	}
	return names
}

// filterByPrefix returns the names that start with prefix.
func filterByPrefix(names []string, prefix string) []string {
	var out []string
	for _, n := range names {
		if strings.HasPrefix(n, prefix) {
			out = append(out, n)
		}
	}
	return out
}

// longestCommonPrefix returns the longest string that prefixes every name.
func longestCommonPrefix(names []string) string {
	if len(names) == 0 {
		return ""
	}
	p := names[0]
	for _, n := range names[1:] {
		for !strings.HasPrefix(n, p) {
			p = p[:len(p)-1]
			if p == "" {
				return ""
			}
		}
	}
	return p
}

// quoteArg double-quotes s when it contains a space so it round-trips through
// tokenize; otherwise it is returned unchanged.
func quoteArg(s string) string {
	if strings.ContainsAny(s, " \t") {
		return `"` + s + `"`
	}
	return s
}

// lastTokenStart returns the byte index in s where the final token begins, or
// len(s) when s ends at a token boundary (so a new token starts there). It
// honors single and double quotes the same way tokenize does.
func lastTokenStart(s string) int {
	start, inToken := 0, false
	var quote rune
	for i, r := range s {
		switch {
		case quote != 0:
			if r == quote {
				quote = 0
			}
		case r == '\'' || r == '"':
			if !inToken {
				start, inToken = i, true
			}
			quote = r
		case r == ' ' || r == '\t':
			inToken = false
		default:
			if !inToken {
				start, inToken = i, true
			}
		}
	}
	if !inToken {
		return len(s)
	}
	return start
}

// dispatch runs one parsed command line; it returns true when the session should
// end.
func (s *Shell) dispatch(args []string) (quit bool) {
	name, rest := args[0], args[1:]
	switch name {
	case "quit", "exit", "bye":
		return true
	}
	cmd, ok := commands[name]
	if !ok {
		fmt.Fprintf(s.out, "%s: unknown command (try 'help')\n", name)
		return false
	}
	if err := cmd.run(s, rest); err != nil {
		if s.jsonOut {
			encodeErrorJSON(s.out, friendlyErr(err))
		} else {
			fmt.Fprintf(s.out, "%s: %v\n", name, friendlyErr(err))
		}
	}
	return false
}

// --- working-directory helpers ---

// pwd renders the current remote directory (the label, or "/" at the virtual
// root) as an absolute path. A message is a leaf, so pwd is never deeper than a
// single label component.
func (s *Shell) pwd() string {
	if len(s.cwd) == 0 {
		return "/"
	}
	var b strings.Builder
	for _, r := range s.cwd {
		b.WriteByte('/')
		b.WriteString(r.Name)
	}
	return b.String()
}

// currentID returns the label ID at the tip of stack, or "" at the virtual root.
func currentID(stack []gmailpkg.Ref) string {
	if len(stack) == 0 {
		return ""
	}
	return stack[len(stack)-1].ID
}

// --- id: addressing ---

const (
	idPrefix       = "id:"        // address a message/attachment directly by ID
	idThreadPrefix = "id:thread:" // address a whole thread (opt-in; rm/get only)
	idAttPrefix    = "id:att:"    // address an attachment: id:att:<msgID>:<attID>
	idDraftPrefix  = "id:draft:"  // address a draft: id:draft:<draftID> (send/attach)
)

// parseIDArg reports whether seg is a bare "id:<ID>" reference (a message ID) and
// returns the ID. The id:thread: and id:att: forms are NOT matched here — they
// are handled by their own parsers — so a message-ID lookup never swallows them.
// An empty ID, or an ID containing "/", is not treated as an ID.
func parseIDArg(seg string) (id string, ok bool) {
	if !strings.HasPrefix(seg, idPrefix) {
		return "", false
	}
	if strings.HasPrefix(seg, idThreadPrefix) || strings.HasPrefix(seg, idAttPrefix) || strings.HasPrefix(seg, idDraftPrefix) {
		return "", false
	}
	id = seg[len(idPrefix):]
	if id == "" || strings.Contains(id, "/") {
		return "", false
	}
	return id, true
}

// parseDraftIDArg resolves a draft reference to its draft ID. It accepts the
// explicit "id:draft:<id>" form and, as a convenience, the bare "id:<id>" form
// (a draft is addressed by id: just like a message). An empty ID, or one
// containing "/", is rejected. The id:thread: and id:att: forms are NOT matched.
func parseDraftIDArg(seg string) (id string, ok bool) {
	if strings.HasPrefix(seg, idDraftPrefix) {
		id = seg[len(idDraftPrefix):]
		if id == "" || strings.Contains(id, "/") {
			return "", false
		}
		return id, true
	}
	return parseIDArg(seg)
}

// parseThreadIDArg reports whether seg is an "id:thread:<ID>" reference and
// returns the thread ID.
func parseThreadIDArg(seg string) (id string, ok bool) {
	if !strings.HasPrefix(seg, idThreadPrefix) {
		return "", false
	}
	id = seg[len(idThreadPrefix):]
	if id == "" || strings.Contains(id, "/") {
		return "", false
	}
	return id, true
}

// parseAttIDArg reports whether seg is an "id:att:<msgID>:<attID>" reference and
// returns the message and attachment IDs.
func parseAttIDArg(seg string) (msgID, attID string, ok bool) {
	if !strings.HasPrefix(seg, idAttPrefix) {
		return "", "", false
	}
	rest := seg[len(idAttPrefix):]
	i := strings.Index(rest, ":")
	if i <= 0 || i == len(rest)-1 {
		return "", "", false
	}
	msgID, attID = rest[:i], rest[i+1:]
	if strings.Contains(attID, ":") {
		return "", "", false
	}
	return msgID, attID, true
}

// resolveDir resolves a path (absolute or relative) to a directory stack in the
// 2-level model. The only navigable container is a label, so a resolved stack is
// either empty (virtual root) or a single label element. "." stays put, ".."
// goes to the root, and any named segment selects a label. A second named
// component is rejected — messages are leaves, not directories.
func (s *Shell) resolveDir(path string) ([]gmailpkg.Ref, error) {
	stack, segs, err := s.startStack(path)
	if err != nil {
		return nil, err
	}
	for _, seg := range segs {
		switch seg {
		case "", ".":
			continue
		case "..":
			if len(stack) > 0 {
				stack = stack[:len(stack)-1]
			}
		default:
			if len(stack) != 0 {
				return nil, fmt.Errorf("%s: not a directory (messages are leaves; cd a label)", seg)
			}
			label, err := s.c.FindLabel(s.ctx, seg)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", seg, err)
			}
			stack = append(stack, gmailpkg.Ref{ID: label.ID, Name: label.Name, Kind: gmailpkg.KindLabel})
		}
	}
	return stack, nil
}

// startStack returns the initial directory stack and the path split into segments
// to walk. A leading "id:<labelID>" segment is not supported for directories
// (labels are addressed by name); an absolute path starts at the virtual root and
// a relative path starts at a copy of the cwd.
func (s *Shell) startStack(path string) ([]gmailpkg.Ref, []string, error) {
	segs := strings.Split(path, "/")
	if strings.HasPrefix(path, "/") {
		return nil, segs, nil
	}
	stack := make([]gmailpkg.Ref, len(s.cwd))
	copy(stack, s.cwd)
	return stack, segs, nil
}

// resolveMessage resolves a path to a single message. A bare "id:<ID>" argument
// is resolved directly by ID, bypassing name navigation. Otherwise the leading
// component (if any) names a label and the final component is a synthesized
// message name looked up within it.
func (s *Shell) resolveMessage(path string) (*gmail.Message, error) {
	if id, ok := parseIDArg(path); ok {
		return s.c.GetMessage(s.ctx, id, "full")
	}
	dir, base := splitPath(path)
	switch base {
	case "":
		return nil, fmt.Errorf("%s: invalid path", path)
	case ".", "..":
		return nil, fmt.Errorf("%q: not a message", base)
	}
	stack := s.cwd
	if dir != "" || strings.HasPrefix(path, "/") {
		var err error
		stack, err = s.resolveDir(dir)
		if err != nil {
			return nil, err
		}
	}
	if len(stack) == 0 {
		return nil, fmt.Errorf("%s: is a label, not a message", base)
	}
	m, err := s.c.FindMessageByName(s.ctx, currentID(stack), base)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", base, err)
	}
	return m, nil
}

// splitPath splits a remote path into its directory part and final element,
// ignoring any trailing slash.
func splitPath(path string) (dir, base string) {
	path = strings.TrimRight(path, "/")
	if i := strings.LastIndex(path, "/"); i >= 0 {
		return path[:i+1], path[i+1:]
	}
	return "", path
}

// tokenize splits a command line into arguments, honoring single and double
// quotes so names may contain spaces.
func tokenize(line string) ([]string, error) {
	var args []string
	var cur strings.Builder
	inToken := false
	var quote rune // 0, '\'' or '"'
	for _, r := range line {
		switch {
		case quote != 0:
			if r == quote {
				quote = 0
			} else {
				cur.WriteRune(r)
			}
		case r == '\'' || r == '"':
			quote = r
			inToken = true
		case r == ' ' || r == '\t' || r == '\r':
			if inToken {
				args = append(args, cur.String())
				cur.Reset()
				inToken = false
			}
		default:
			cur.WriteRune(r)
			inToken = true
		}
	}
	if quote != 0 {
		return nil, fmt.Errorf("unterminated %c quote", quote)
	}
	if inToken {
		args = append(args, cur.String())
	}
	return args, nil
}

// sortedCommandNames returns the command verbs in alphabetical order.
func sortedCommandNames() []string {
	names := make([]string, 0, len(commands))
	for n := range commands {
		names = append(names, n)
	}
	sort.Strings(names)
	return names
}
