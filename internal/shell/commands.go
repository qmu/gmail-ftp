package shell

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"gmail-ftp/internal/audit"
	gmailpkg "gmail-ftp/internal/gmail"

	gmail "google.golang.org/api/gmail/v1"
)

func init() {
	commands = map[string]command{
		"ls":      {run: (*Shell).cmdLs, usage: "ls [dir]", help: "list labels (at root) or a label's messages"},
		"cd":      {run: (*Shell).cmdCd, usage: "cd [label]", help: "enter a label (no arg: go to the root)"},
		"pwd":     {run: (*Shell).cmdPwd, usage: "pwd", help: "print the remote working directory"},
		"find":    {run: (*Shell).cmdFind, usage: "find <pattern> [label]", help: "search message subjects by substring"},
		"search":  {run: (*Shell).cmdSearch, usage: "search <gmail-query>", help: "search with raw Gmail query syntax"},
		"get":     {run: (*Shell).cmdGet, usage: "get <remote> [local]", help: "download a message (.eml) or attachment"},
		"put":     {run: (*Shell).cmdPut, usage: "put <local> [remote]", help: "create a draft from a local file (never sends)"},
		"mkdir":   {run: (*Shell).cmdMkdir, usage: "mkdir <name>", help: "create a Gmail user label"},
		"rm":      {run: (*Shell).cmdRm, usage: "rm <name>", help: "trash a single message (reversible)"},
		"send":    {run: (*Shell).cmdSend, usage: "send <draft>", help: "[deferred to v1.1] send a draft"},
		"label":   {run: (*Shell).cmdLabel, usage: "label <msg> <name>", help: "[deferred to v1.1] add a label to a message"},
		"unlabel": {run: (*Shell).cmdUnlabel, usage: "unlabel <msg> <name>", help: "[deferred to v1.1] remove a label from a message"},
		"lcd":     {run: (*Shell).cmdLcd, usage: "lcd [dir]", help: "change the local working directory"},
		"lls":     {run: (*Shell).cmdLls, usage: "lls [dir]", help: "list a local directory"},
		"lpwd":    {run: (*Shell).cmdLpwd, usage: "lpwd", help: "print the local working directory"},
		"help":    {run: (*Shell).cmdHelp, usage: "help [cmd]", help: "show command help"},
		"?":       {run: (*Shell).cmdHelp, usage: "? [cmd]", help: "alias for help"},
	}
}

func (s *Shell) cmdLs(args []string) error {
	stack := s.cwd
	if len(args) > 0 {
		arg := args[0]
		// An id:att:/id: argument or a message-name argument shows that single
		// leaf; a pure label reference (/, ., .., trailing slash, bare label name)
		// lists a label's messages.
		if msgID, attID, ok := parseAttIDArg(arg); ok {
			return s.lsAttachmentByID(msgID, attID)
		}
		_, base := splitPath(arg)
		if base == "" || base == "." || base == ".." || strings.HasSuffix(arg, "/") || s.singleLabelArg(arg) {
			var err error
			if stack, err = s.resolveDir(arg); err != nil {
				return err
			}
		} else {
			// A message reference: list the message's attachments (its leaves).
			m, err := s.resolveMessage(arg)
			if err != nil {
				return err
			}
			return s.lsAttachments(m)
		}
	}
	// The virtual root lists the labels, not a label's messages.
	if len(stack) == 0 {
		return s.listLabels()
	}
	list, err := s.c.ListMessages(s.ctx, currentID(stack), "")
	if err != nil {
		return err
	}
	entries := make([]entry, 0, len(list.Messages))
	for _, m := range list.Messages {
		entries = append(entries, messageEntry(m))
	}
	return s.emit(entries, func() {
		for _, m := range list.Messages {
			s.printMessageRow(m)
		}
		if list.Truncated {
			fmt.Fprintf(s.out, "… showing first %d — use 'search' or a narrower label for more\n", len(list.Messages))
		}
	})
}

// printMessageRow prints one message listing row: unread flag, date, sender, and
// the synthesized name.
func (s *Shell) printMessageRow(m *gmail.Message) {
	flag := " "
	if gmailpkg.Unread(m) {
		flag = "*"
	}
	date := ""
	if d := gmailpkg.Date(m); !d.IsZero() {
		date = d.Local().Format("2006-01-02 15:04")
	}
	fmt.Fprintf(s.out, "%s %-16s  %-24s  %s\n", flag, date, truncate(gmailpkg.Header(m, "From"), 24), gmailpkg.MessageName(m))
}

// listLabels prints the virtual-root entries (the user's labels) as containers.
func (s *Shell) listLabels() error {
	labels, err := s.c.ListLabels(s.ctx)
	if err != nil {
		return err
	}
	entries := make([]entry, 0, len(labels))
	for _, l := range labels {
		entries = append(entries, labelEntry(l))
	}
	return s.emit(entries, func() {
		for _, l := range labels {
			fmt.Fprintf(s.out, "%s/\n", l.Name)
		}
	})
}

// lsAttachments lists a message's attachment leaves.
func (s *Shell) lsAttachments(m *gmail.Message) error {
	atts := gmailpkg.Attachments(m)
	entries := make([]entry, 0, len(atts))
	for _, a := range atts {
		addr := fmt.Sprintf("%s%s:%s", idAttPrefix, m.Id, a.AttachmentID)
		entries = append(entries, attachmentEntry(a, addr))
	}
	return s.emit(entries, func() {
		for _, a := range atts {
			fmt.Fprintf(s.out, "%12s  %s\n", byteCount(a.Size), a.Filename)
		}
	})
}

// lsAttachmentByID lists a single attachment referenced by id:att:<msg>:<att>.
func (s *Shell) lsAttachmentByID(msgID, attID string) error {
	m, err := s.c.GetMessage(s.ctx, msgID, "full")
	if err != nil {
		return err
	}
	for _, a := range gmailpkg.Attachments(m) {
		if a.AttachmentID == attID {
			addr := fmt.Sprintf("%s%s:%s", idAttPrefix, msgID, attID)
			return s.emit([]entry{attachmentEntry(a, addr)}, func() {
				fmt.Fprintf(s.out, "%12s  %s\n", byteCount(a.Size), a.Filename)
			})
		}
	}
	return gmailpkg.ErrNotFound
}

// singleLabelArg reports whether arg selects a single top-level label (a
// directory at the virtual root), so ls lists it rather than treating it as a
// message name.
func (s *Shell) singleLabelArg(arg string) bool {
	trimmed := strings.Trim(arg, "/")
	if trimmed == "" {
		return false
	}
	if strings.HasPrefix(arg, "/") {
		// /Label or /Work/Receipts selects a label regardless of cwd (a nested
		// label is still a single label name in Gmail).
		return true
	}
	return len(s.cwd) == 0 // a bare name at the virtual root is a label
}

func (s *Shell) cmdCd(args []string) error {
	target := "/"
	if len(args) > 0 {
		target = args[0]
	}
	stack, err := s.resolveDir(target)
	if err != nil {
		return err
	}
	s.cwd = stack
	return nil
}

func (s *Shell) cmdPwd(args []string) error {
	return s.emit(pwdResult{Path: s.pwd()}, func() {
		fmt.Fprintln(s.out, s.pwd())
	})
}

func (s *Shell) cmdGet(args []string) error {
	if len(args) < 1 {
		return usageErr("get <remote> [local]")
	}
	arg := args[0]
	local := ""
	if len(args) >= 2 {
		local = args[1]
	}

	// id:att:<msg>:<att> — download one attachment's raw bytes.
	if msgID, attID, ok := parseAttIDArg(arg); ok {
		return s.getAttachment(msgID, attID, local)
	}
	// id:thread:<id> — export the whole thread as .mbox (power-user opt-in).
	if tid, ok := parseThreadIDArg(arg); ok {
		return s.getThread(tid, local)
	}

	m, err := s.resolveMessage(arg)
	if err != nil {
		return err
	}
	// A message downloads as raw RFC 822 (.eml). A "name.txt" local target (or a
	// .txt remote hint) triggers the readable text export instead.
	asText := strings.HasSuffix(strings.ToLower(local), ".txt")
	name := gmailpkg.MessageName(m)
	var write func(io.Writer) (int64, error)
	if asText {
		name += ".txt"
		write = func(w io.Writer) (int64, error) {
			full, err := s.c.GetMessage(s.ctx, m.Id, "full")
			if err != nil {
				return 0, err
			}
			n, err := w.Write(gmailpkg.RenderText(full))
			return int64(n), err
		}
	} else {
		name += ".eml"
		write = func(w io.Writer) (int64, error) {
			raw, err := s.c.GetRawMessage(s.ctx, m.Id)
			if err != nil {
				return 0, err
			}
			n, err := w.Write(raw)
			return int64(n), err
		}
	}
	dest, n, err := saveToFile(resolveLocalDest(local, name), write)
	if err != nil {
		return err
	}
	return s.emit(actionResult{Action: "downloaded", Name: name, ID: m.Id, Dest: dest, Size: n, ThreadID: m.ThreadId}, func() {
		fmt.Fprintf(s.out, "downloaded %s -> %s (%s)\n", name, dest, byteCount(n))
	})
}

// getAttachment downloads one attachment's raw bytes to a local file.
func (s *Shell) getAttachment(msgID, attID, local string) error {
	m, err := s.c.GetMessage(s.ctx, msgID, "full")
	if err != nil {
		return err
	}
	var found *gmailpkg.Attachment
	for _, a := range gmailpkg.Attachments(m) {
		if a.AttachmentID == attID {
			a := a
			found = &a
			break
		}
	}
	if found == nil {
		return gmailpkg.ErrNotFound
	}
	write := func(w io.Writer) (int64, error) {
		data, err := s.c.GetAttachment(s.ctx, msgID, attID)
		if err != nil {
			return 0, err
		}
		n, err := w.Write(data)
		return int64(n), err
	}
	dest, n, err := saveToFile(resolveLocalDest(local, found.Filename), write)
	if err != nil {
		return err
	}
	return s.emit(actionResult{Action: "downloaded", Name: found.Filename, ID: attID, Dest: dest, Size: n}, func() {
		fmt.Fprintf(s.out, "downloaded %s -> %s (%s)\n", found.Filename, dest, byteCount(n))
	})
}

// getThread exports a whole thread as a concatenated .mbox of each message's raw
// bytes (the opt-in power-user path behind id:thread:<id>).
func (s *Shell) getThread(tid, local string) error {
	t, err := s.c.GetThread(s.ctx, tid)
	if err != nil {
		return err
	}
	name := "thread-" + tid + ".mbox"
	write := func(w io.Writer) (int64, error) {
		var total int64
		for _, m := range t.Messages {
			raw, err := s.c.GetRawMessage(s.ctx, m.Id)
			if err != nil {
				return total, err
			}
			// Minimal mbox framing: a "From " separator line precedes each message.
			sep := []byte("From - \n")
			n, _ := w.Write(sep)
			total += int64(n)
			n, err = w.Write(raw)
			total += int64(n)
			if err != nil {
				return total, err
			}
			n, _ = w.Write([]byte("\n"))
			total += int64(n)
		}
		return total, nil
	}
	dest, n, err := saveToFile(resolveLocalDest(local, name), write)
	if err != nil {
		return err
	}
	return s.emit(actionResult{Action: "downloaded", Name: name, ID: tid, Dest: dest, Size: n, ThreadID: tid}, func() {
		fmt.Fprintf(s.out, "downloaded thread %s -> %s (%s)\n", tid, dest, byteCount(n))
	})
}

// resolveLocalDest decides the local path for a download. An empty local writes
// name into the current directory; a local naming an existing directory (or one
// ending in a path separator) places name inside it; otherwise local is used
// verbatim.
func resolveLocalDest(local, name string) string {
	if local == "" {
		return name
	}
	if strings.HasSuffix(local, "/") || strings.HasSuffix(local, string(os.PathSeparator)) {
		return filepath.Join(local, name)
	}
	if info, err := os.Stat(local); err == nil && info.IsDir() {
		return filepath.Join(local, name)
	}
	return local
}

// saveToFile streams write's output to dest atomically: it writes to a temp file
// in the destination directory and renames it into place only after the transfer
// and close both succeed. An interrupted or failed transfer therefore never
// truncates or overwrites an existing good file.
func saveToFile(dest string, write func(io.Writer) (int64, error)) (string, int64, error) {
	tmp, err := os.CreateTemp(filepath.Dir(dest), "."+filepath.Base(dest)+".part-*")
	if err != nil {
		return "", 0, err
	}
	tmpName := tmp.Name()
	committed := false
	defer func() {
		if !committed {
			tmp.Close()
			os.Remove(tmpName)
		}
	}()

	n, err := write(tmp)
	if err != nil {
		return "", 0, err
	}
	if err := tmp.Close(); err != nil {
		return "", 0, err
	}
	if err := os.Chmod(tmpName, 0o644); err != nil {
		return "", 0, err
	}
	if err := os.Rename(tmpName, dest); err != nil {
		return "", 0, err
	}
	committed = true
	return dest, n, nil
}

// cmdPut creates a DRAFT from a local RFC 5322 file. It never sends — sending is
// the explicit (deferred) send verb. Returns the new draft's id.
func (s *Shell) cmdPut(args []string) error {
	if len(args) < 1 {
		return usageErr("put <local> [remote]")
	}
	local := args[0]
	in, err := os.Open(local)
	if err != nil {
		return err
	}
	defer in.Close()
	if st, err := in.Stat(); err != nil {
		return err
	} else if st.IsDir() {
		return fmt.Errorf("%s is a directory (recursive put is not supported)", local)
	}
	raw, err := io.ReadAll(in)
	if err != nil {
		return err
	}
	draft, err := s.c.CreateDraft(s.ctx, raw)
	if err != nil {
		return err
	}
	threadID := ""
	if draft.Message != nil {
		threadID = draft.Message.ThreadId
	}
	s.audit(audit.Entry{Op: audit.OpDraft, Name: filepath.Base(local), ID: draft.Id, ThreadID: threadID, Cwd: s.pwd(), Size: int64(len(raw))})
	return s.emit(actionResult{Action: "drafted", Name: filepath.Base(local), ID: draft.Id, ThreadID: threadID, Size: int64(len(raw))}, func() {
		fmt.Fprintf(s.out, "drafted %s (draft %s) — not sent\n", local, draft.Id)
	})
}

// cmdMkdir creates a Gmail user label (the directory-creation analogue).
func (s *Shell) cmdMkdir(args []string) error {
	if len(args) < 1 {
		return usageErr("mkdir <name>")
	}
	name := args[0]
	if _, ok := parseIDArg(name); ok {
		return fmt.Errorf("%s: mkdir takes a label name, not an id:", name)
	}
	l, err := s.c.CreateLabel(s.ctx, name)
	if err != nil {
		return err
	}
	s.audit(audit.Entry{Op: audit.OpMkLabel, Name: l.Name, ID: l.Id, Cwd: s.pwd()})
	return s.emit(actionResult{Action: "created", Name: l.Name, ID: l.Id}, func() {
		fmt.Fprintf(s.out, "created label %s (%s)\n", l.Name, l.Id)
	})
}

// cmdRm trashes a SINGLE message by default (reversible). A whole thread is never
// trashed implicitly — only via the explicit rm id:thread:<id> opt-in.
func (s *Shell) cmdRm(args []string) error {
	if len(args) < 1 {
		return usageErr("rm <name>")
	}
	arg := args[0]
	// Explicit thread-trash opt-in.
	if tid, ok := parseThreadIDArg(arg); ok {
		if _, err := s.c.TrashThread(s.ctx, tid); err != nil {
			return err
		}
		s.audit(audit.Entry{Op: audit.OpTrash, Name: "thread " + tid, ID: tid, ThreadID: tid, Cwd: s.pwd()})
		return s.emit(actionResult{Action: "trashed", Name: "thread " + tid, ID: tid, ThreadID: tid}, func() {
			fmt.Fprintf(s.out, "trashed thread %s\n", tid)
		})
	}
	// Default: trash exactly one message.
	m, err := s.resolveMessage(arg)
	if err != nil {
		return err
	}
	if _, err := s.c.TrashMessage(s.ctx, m.Id); err != nil {
		return err
	}
	name := gmailpkg.MessageName(m)
	s.audit(audit.Entry{Op: audit.OpTrash, Name: name, ID: m.Id, ThreadID: m.ThreadId, Cwd: s.pwd(), Size: m.SizeEstimate})
	return s.emit(actionResult{Action: "trashed", Name: name, ID: m.Id, ThreadID: m.ThreadId}, func() {
		fmt.Fprintf(s.out, "trashed %s\n", name)
	})
}

// cmdFind searches message subjects for a case-insensitive substring within the
// current label (or an optional [label] anchor), printing each match. find never
// mutates; act on a match by its id:.
func (s *Shell) cmdFind(args []string) error {
	if len(args) < 1 {
		return usageErr("find <pattern> [label]")
	}
	pattern := args[0]
	stack := s.cwd
	if len(args) >= 2 {
		var err error
		if stack, err = s.resolveDir(args[1]); err != nil {
			return err
		}
	}
	list, err := s.c.ListMessages(s.ctx, currentID(stack), "")
	if err != nil {
		return err
	}
	entries := []entry{}
	for _, m := range list.Messages {
		if !gmailpkg.NameContains(gmailpkg.MessageName(m), pattern) &&
			!gmailpkg.NameContains(gmailpkg.Header(m, "Subject"), pattern) {
			continue
		}
		e := messageEntry(m)
		e.Path = s.pwd() + "/" + e.Name
		entries = append(entries, e)
	}
	return s.emit(entries, func() {
		for _, m := range list.Messages {
			if !gmailpkg.NameContains(gmailpkg.MessageName(m), pattern) &&
				!gmailpkg.NameContains(gmailpkg.Header(m, "Subject"), pattern) {
				continue
			}
			s.printMessageRow(m)
		}
	})
}

// cmdSearch runs a raw Gmail search query (from:/subject:/is:unread syntax)
// across the mailbox and prints the matching messages.
func (s *Shell) cmdSearch(args []string) error {
	if len(args) < 1 {
		return usageErr("search <gmail-query>")
	}
	query := strings.Join(args, " ")
	list, err := s.c.Search(s.ctx, query)
	if err != nil {
		return err
	}
	entries := make([]entry, 0, len(list.Messages))
	for _, m := range list.Messages {
		entries = append(entries, messageEntry(m))
	}
	return s.emit(entries, func() {
		for _, m := range list.Messages {
			s.printMessageRow(m)
		}
		if list.Truncated {
			fmt.Fprintf(s.out, "… showing first %d — refine the query for more\n", len(list.Messages))
		}
	})
}

// cmdSend is DEFERRED to v1.1. It is defined so the verb exists and is discovered
// by help/completion, but it never sends mail in v1 — it returns a clear notice.
func (s *Shell) cmdSend(args []string) error {
	return fmt.Errorf("send is deferred to v1.1 — v1 never sends; use 'put' to create a draft and send it from Gmail")
}

// cmdLabel is DEFERRED to v1.1 (message-level label add). Stubbed, not wired.
func (s *Shell) cmdLabel(args []string) error {
	return fmt.Errorf("label is deferred to v1.1 — use 'mkdir' to create a label; message-level labeling ships later")
}

// cmdUnlabel is DEFERRED to v1.1 (message-level label remove). Stubbed.
func (s *Shell) cmdUnlabel(args []string) error {
	return fmt.Errorf("unlabel is deferred to v1.1 — message-level labeling ships later")
}

func (s *Shell) cmdLcd(args []string) error {
	dir := ""
	if len(args) > 0 {
		dir = args[0]
	}
	if dir == "" {
		if home, err := os.UserHomeDir(); err == nil {
			dir = home
		} else {
			return err
		}
	}
	if err := os.Chdir(dir); err != nil {
		return err
	}
	return s.cmdLpwd(nil)
}

func (s *Shell) cmdLls(args []string) error {
	dir := "."
	if len(args) > 0 {
		dir = args[0]
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return err
	}
	for _, e := range entries {
		name := e.Name()
		size := "-"
		if info, err := e.Info(); err == nil && !e.IsDir() {
			size = byteCount(info.Size())
		}
		if e.IsDir() {
			name += "/"
		}
		fmt.Fprintf(s.out, "%12s  %s\n", size, name)
	}
	return nil
}

func (s *Shell) cmdLpwd(args []string) error {
	wd, err := os.Getwd()
	if err != nil {
		return err
	}
	fmt.Fprintln(s.out, wd)
	return nil
}

func (s *Shell) cmdHelp(args []string) error {
	if len(args) > 0 {
		switch args[0] {
		case "quit", "exit", "bye":
			fmt.Fprintln(s.out, "quit | exit | bye      end the session")
			return nil
		}
		if c, ok := commands[args[0]]; ok {
			fmt.Fprintf(s.out, "%-24s %s\n", c.usage, c.help)
			return nil
		}
		return fmt.Errorf("no such command %q", args[0])
	}
	fmt.Fprintln(s.out, "Commands:")
	for _, name := range sortedCommandNames() {
		c := commands[name]
		fmt.Fprintf(s.out, "  %-24s %s\n", c.usage, c.help)
	}
	fmt.Fprintln(s.out, "  quit | exit | bye        end the session")
	fmt.Fprintln(s.out, "Navigation is two levels: root lists labels, a label lists messages,")
	fmt.Fprintln(s.out, "a message contains attachments. Address items directly by id::")
	fmt.Fprintln(s.out, "  get id:<msgID>           download a message (.eml)")
	fmt.Fprintln(s.out, "  rm  id:<msgID>           trash a single message")
	fmt.Fprintln(s.out, "  get id:att:<msg>:<att>   download one attachment")
	fmt.Fprintln(s.out, "  get/rm id:thread:<id>    export/trash a whole thread (opt-in)")
	return nil
}

// --- formatting helpers ---

func usageErr(usage string) error { return fmt.Errorf("usage: %s", usage) }

// truncate shortens s to at most n runes, appending an ellipsis when cut.
func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n <= 1 {
		return string(r[:n])
	}
	return string(r[:n-1]) + "…"
}

// byteCount renders n as a human-readable size.
func byteCount(n int64) string {
	const unit = 1024
	if n < unit {
		return strconv.FormatInt(n, 10) + "B"
	}
	div, exp := int64(unit), 0
	for x := n / unit; x >= unit; x /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f%cB", float64(n)/float64(div), "KMGTPE"[exp])
}
