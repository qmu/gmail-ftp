package shell

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	gmailpkg "gmail-ftp/internal/gmail"

	gmail "google.golang.org/api/gmail/v1"
	"google.golang.org/api/googleapi"
)

// --- fake gmail client ---

// fakeClient is an in-memory gmailClient used to unit-test command dispatch and
// output without live Gmail credentials. It records the mutating calls made so
// safety properties (e.g. rm trashes a single message, never a thread) can be
// asserted.
type fakeClient struct {
	labels   []gmailpkg.Label
	messages map[string][]*gmail.Message // labelID -> messages
	byID     map[string]*gmail.Message

	trashedMessages []string
	trashedThreads  []string
	createdLabels   []string
	createdDrafts   int
	modifyCalls     int
}

func (f *fakeClient) ListLabels(ctx context.Context) ([]gmailpkg.Label, error) {
	return f.labels, nil
}

func (f *fakeClient) ListMessages(ctx context.Context, labelID, query string) (*gmailpkg.MessageList, error) {
	return &gmailpkg.MessageList{Messages: f.messages[labelID]}, nil
}

func (f *fakeClient) Search(ctx context.Context, query string) (*gmailpkg.MessageList, error) {
	var all []*gmail.Message
	for _, m := range f.byID {
		all = append(all, m)
	}
	return &gmailpkg.MessageList{Messages: all}, nil
}

func (f *fakeClient) GetMessage(ctx context.Context, id, format string) (*gmail.Message, error) {
	if m, ok := f.byID[id]; ok {
		return m, nil
	}
	return nil, gmailpkg.ErrNotFound
}

func (f *fakeClient) GetRawMessage(ctx context.Context, id string) ([]byte, error) {
	if _, ok := f.byID[id]; ok {
		return []byte("Subject: raw\r\n\r\nbody"), nil
	}
	return nil, gmailpkg.ErrNotFound
}

func (f *fakeClient) GetAttachment(ctx context.Context, msgID, attID string) ([]byte, error) {
	return []byte("ATTACHMENT-BYTES"), nil
}

func (f *fakeClient) GetThread(ctx context.Context, id string) (*gmail.Thread, error) {
	return &gmail.Thread{Id: id, Messages: []*gmail.Message{{Id: "m1"}, {Id: "m2"}}}, nil
}

func (f *fakeClient) CreateDraft(ctx context.Context, raw []byte) (*gmail.Draft, error) {
	f.createdDrafts++
	return &gmail.Draft{Id: "draft1", Message: &gmail.Message{Id: "dm1", ThreadId: "dt1"}}, nil
}

func (f *fakeClient) TrashMessage(ctx context.Context, id string) (*gmail.Message, error) {
	f.trashedMessages = append(f.trashedMessages, id)
	return f.byID[id], nil
}

func (f *fakeClient) TrashThread(ctx context.Context, id string) (*gmail.Thread, error) {
	f.trashedThreads = append(f.trashedThreads, id)
	return &gmail.Thread{Id: id}, nil
}

func (f *fakeClient) ModifyLabels(ctx context.Context, id string, add, remove []string) (*gmail.Message, error) {
	f.modifyCalls++
	return f.byID[id], nil
}

func (f *fakeClient) CreateLabel(ctx context.Context, name string) (*gmail.Label, error) {
	f.createdLabels = append(f.createdLabels, name)
	return &gmail.Label{Id: "Label_new", Name: name}, nil
}

func (f *fakeClient) FindLabel(ctx context.Context, name string) (gmailpkg.Label, error) {
	for _, l := range f.labels {
		if l.Name == name {
			return l, nil
		}
	}
	return gmailpkg.Label{}, gmailpkg.ErrNotFound
}

func (f *fakeClient) FindMessageByName(ctx context.Context, labelID, name string) (*gmail.Message, error) {
	var match []*gmail.Message
	for _, m := range f.messages[labelID] {
		if gmailpkg.MessageName(m) == name {
			match = append(match, m)
		}
	}
	switch len(match) {
	case 0:
		return nil, gmailpkg.ErrNotFound
	case 1:
		return match[0], nil
	default:
		return nil, gmailpkg.ErrAmbiguous
	}
}

// hdr builds a synthetic message with the given id, threadId, internalDate (ms),
// labels, and a Subject header.
func hdr(id, threadID string, millis int64, labels []string, subject string) *gmail.Message {
	return &gmail.Message{
		Id:           id,
		ThreadId:     threadID,
		InternalDate: millis,
		LabelIds:     labels,
		Payload:      &gmail.MessagePart{Headers: []*gmail.MessagePartHeader{{Name: "Subject", Value: subject}}},
	}
}

func newFake() *fakeClient {
	m1 := hdr("m1", "t1", 0, []string{"INBOX", "UNREAD"}, "Hello")
	m2 := hdr("m2", "t2", 0, []string{"INBOX"}, "Invoice")
	return &fakeClient{
		labels: []gmailpkg.Label{
			{ID: "INBOX", Name: "INBOX", System: true},
			{ID: "Label_1", Name: "Work", System: false},
		},
		messages: map[string][]*gmail.Message{"INBOX": {m1, m2}},
		byID:     map[string]*gmail.Message{"m1": m1, "m2": m2},
	}
}

func newShell(f *fakeClient, jsonOut bool) (*Shell, *bytes.Buffer) {
	var buf bytes.Buffer
	return &Shell{ctx: context.Background(), c: f, out: &buf, jsonOut: jsonOut}, &buf
}

// --- pure-function tests (copied/adapted from gdrive-ftp) ---

func TestFilterByPrefix(t *testing.T) {
	names := []string{"INBOX/", "IMPORTANT/", "budget", "notes"}
	got := filterByPrefix(names, "I")
	want := []string{"INBOX/", "IMPORTANT/"}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("filterByPrefix = %#v, want %#v", got, want)
	}
	if got := filterByPrefix(names, ""); len(got) != 4 {
		t.Errorf("empty prefix should match all, got %d", len(got))
	}
	if got := filterByPrefix(names, "zzz"); got != nil {
		t.Errorf("no match should be nil, got %#v", got)
	}
}

func TestLongestCommonPrefix(t *testing.T) {
	tests := []struct {
		in   []string
		want string
	}{
		{[]string{"INBOX/", "IMPORTANT/"}, "I"},
		{[]string{"abc", "abd", "abz"}, "ab"},
		{[]string{"only/"}, "only/"},
		{[]string{"a", "b"}, ""},
		{nil, ""},
	}
	for _, tt := range tests {
		if got := longestCommonPrefix(tt.in); got != tt.want {
			t.Errorf("longestCommonPrefix(%v) = %q, want %q", tt.in, got, tt.want)
		}
	}
}

func TestQuoteArg(t *testing.T) {
	if got := quoteArg("plain"); got != "plain" {
		t.Errorf("quoteArg(plain) = %q, want plain", got)
	}
	if got := quoteArg("my label"); got != `"my label"` {
		t.Errorf(`quoteArg("my label") = %q, want "my label"`, got)
	}
}

func TestLastTokenStart(t *testing.T) {
	tests := []struct {
		in   string
		want int
	}{
		{"", 0},
		{"ls", 0},
		{"ls ", 3},
		{"ls IN", 3},
		{"cd a/b/c", 3},
		{`get "my fi`, 4},
	}
	for _, tt := range tests {
		if got := lastTokenStart(tt.in); got != tt.want {
			t.Errorf("lastTokenStart(%q) = %d, want %d", tt.in, got, tt.want)
		}
	}
}

func TestArgKind(t *testing.T) {
	tests := []struct {
		verb string
		idx  int
		want string
	}{
		{"ls", 1, "remote"},
		{"cd", 1, "remote"},
		{"get", 1, "remote"},
		{"get", 2, "local"},
		{"put", 1, "local"},
		{"put", 2, "remote"},
		{"lcd", 1, "local"},
		{"pwd", 1, ""},
		{"ls", 2, ""},
		{"find", 1, ""},        // arg 1 is the pattern, no completion
		{"find", 2, "remote"},  // arg 2 is the label anchor
		{"search", 1, ""},      // arg 1 is the query, no completion
		{"label", 1, "remote"}, // target message
		{"label", 2, ""},       // label name, no completion
		{"send", 1, "local"},
	}
	for _, tt := range tests {
		if got := argKind(tt.verb, tt.idx); got != tt.want {
			t.Errorf("argKind(%q,%d) = %q, want %q", tt.verb, tt.idx, got, tt.want)
		}
	}
}

func TestCompletionVerbs(t *testing.T) {
	verbs := completionVerbs()
	for _, want := range []string{"ls", "cd", "get", "put", "rm", "mkdir", "find", "quit", "exit", "bye"} {
		found := false
		for _, v := range verbs {
			if v == want {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("completionVerbs missing %q", want)
		}
	}
}

func TestFriendlyErr(t *testing.T) {
	if friendlyErr(nil) != nil {
		t.Error("friendlyErr(nil) should be nil")
	}
	plain := errors.New("boom")
	if got := friendlyErr(plain); got != plain {
		t.Errorf("plain error should pass through unchanged, got %v", got)
	}
	disabled := &googleapi.Error{
		Code: 403,
		Message: "Gmail API has not been used in project 123456789012 before or it is " +
			"disabled. Enable it by visiting https://console.developers.google.com/apis/api/" +
			"gmail.googleapis.com/overview?project=123456789012 then retry.",
		Errors: []googleapi.ErrorItem{{Reason: "accessNotConfigured"}},
	}
	got := friendlyErr(disabled)
	if got == disabled || !strings.Contains(got.Error(), "Gmail API is disabled") {
		t.Errorf("disabled-API error not rewritten: %v", got)
	}
	if !strings.Contains(got.Error(), "overview?project=123456789012") {
		t.Errorf("rewritten error should include the exact activation URL: %v", got)
	}
	if !strings.Contains(got.Error(), "--project=123456789012") {
		t.Errorf("rewritten error should include the exact project number: %v", got)
	}
}

// --- label/message stack tests (the 2-level model; no thread frame) ---

var (
	rootStack  []gmailpkg.Ref // virtual root
	labelStack = []gmailpkg.Ref{{ID: "INBOX", Name: "INBOX", Kind: gmailpkg.KindLabel}}
)

func TestPwd(t *testing.T) {
	tests := []struct {
		name string
		cwd  []gmailpkg.Ref
		want string
	}{
		{"virtual root", rootStack, "/"},
		{"a label", labelStack, "/INBOX"},
	}
	for _, tt := range tests {
		s := &Shell{cwd: tt.cwd}
		if got := s.pwd(); got != tt.want {
			t.Errorf("%s: pwd() = %q, want %q", tt.name, got, tt.want)
		}
	}
}

func TestCurrentID(t *testing.T) {
	if got := currentID(rootStack); got != "" {
		t.Errorf("currentID(root) = %q, want empty", got)
	}
	if got := currentID(labelStack); got != "INBOX" {
		t.Errorf("currentID(label) = %q, want INBOX", got)
	}
}

func TestSingleLabelArg(t *testing.T) {
	tests := []struct {
		name string
		cwd  []gmailpkg.Ref
		arg  string
		want bool
	}{
		{"bare name at root", rootStack, "INBOX", true},
		{"absolute single component", labelStack, "/Work", true},
		{"bare name inside a label is a message", labelStack, "Hello", false},
		{"root slash is not a label", rootStack, "/", false},
	}
	for _, tt := range tests {
		s := &Shell{cwd: tt.cwd}
		if got := s.singleLabelArg(tt.arg); got != tt.want {
			t.Errorf("%s: singleLabelArg(%q) = %v, want %v", tt.name, tt.arg, got, tt.want)
		}
	}
}

func TestTokenize(t *testing.T) {
	tests := []struct {
		in   string
		want []string
	}{
		{"", nil},
		{"   ", nil},
		{"ls", []string{"ls"}},
		{"  ls   /INBOX  ", []string{"ls", "/INBOX"}},
		{`get "2026-06-18 Quarterly report" out.eml`, []string{"get", "2026-06-18 Quarterly report", "out.eml"}},
		{`search 'from:a@b is:unread'`, []string{"search", "from:a@b is:unread"}},
		{`cd ""`, []string{"cd", ""}},
		{"a\tb", []string{"a", "b"}},
	}
	for _, tt := range tests {
		got, err := tokenize(tt.in)
		if err != nil {
			t.Errorf("tokenize(%q) unexpected error: %v", tt.in, err)
			continue
		}
		if !reflect.DeepEqual(got, tt.want) {
			t.Errorf("tokenize(%q) = %#v, want %#v", tt.in, got, tt.want)
		}
	}
}

func TestTokenizeUnterminated(t *testing.T) {
	if _, err := tokenize(`get "oops`); err == nil {
		t.Errorf("expected error for unterminated quote")
	}
}

func TestSplitPath(t *testing.T) {
	tests := []struct {
		in        string
		dir, base string
	}{
		{"foo", "", "foo"},
		{"/foo", "/", "foo"},
		{"a/b/c", "a/b/", "c"},
		{"/a/b/c", "/a/b/", "c"},
		{"foo/", "", "foo"},
		{"/", "", ""},
	}
	for _, tt := range tests {
		dir, base := splitPath(tt.in)
		if dir != tt.dir || base != tt.base {
			t.Errorf("splitPath(%q) = (%q, %q), want (%q, %q)", tt.in, dir, base, tt.dir, tt.base)
		}
	}
}

func TestParseIDArg(t *testing.T) {
	tests := []struct {
		in     string
		wantID string
		wantOK bool
	}{
		{"id:18f1a2b", "18f1a2b", true},
		{"id:0Bx_-Msg", "0Bx_-Msg", true},
		{"id:", "", false},          // empty ID
		{"id:a/b", "", false},       // contains a slash → path, not ID
		{"id:thread:t1", "", false}, // thread form handled elsewhere
		{"id:att:m1:a1", "", false}, // attachment form handled elsewhere
		{"Hello", "", false},        // a plain name
		{"ID:18f1a2b", "", false},   // prefix is case-sensitive
		{"xid:18f1a2b", "", false},  // prefix must be at the start
	}
	for _, tt := range tests {
		gotID, gotOK := parseIDArg(tt.in)
		if gotID != tt.wantID || gotOK != tt.wantOK {
			t.Errorf("parseIDArg(%q) = (%q, %v), want (%q, %v)", tt.in, gotID, gotOK, tt.wantID, tt.wantOK)
		}
	}
}

func TestParseThreadIDArg(t *testing.T) {
	tests := []struct {
		in     string
		wantID string
		wantOK bool
	}{
		{"id:thread:t1", "t1", true},
		{"id:thread:", "", false},
		{"id:thread:a/b", "", false},
		{"id:m1", "", false},
	}
	for _, tt := range tests {
		gotID, gotOK := parseThreadIDArg(tt.in)
		if gotID != tt.wantID || gotOK != tt.wantOK {
			t.Errorf("parseThreadIDArg(%q) = (%q, %v), want (%q, %v)", tt.in, gotID, gotOK, tt.wantID, tt.wantOK)
		}
	}
}

func TestParseAttIDArg(t *testing.T) {
	tests := []struct {
		in               string
		wantMsg, wantAtt string
		wantOK           bool
	}{
		{"id:att:m1:a1", "m1", "a1", true},
		{"id:att:m1:", "", "", false},
		{"id:att::a1", "", "", false},
		{"id:att:m1", "", "", false},
		{"id:att:m1:a1:x", "", "", false}, // too many colons
		{"id:m1", "", "", false},
	}
	for _, tt := range tests {
		gotMsg, gotAtt, gotOK := parseAttIDArg(tt.in)
		if gotMsg != tt.wantMsg || gotAtt != tt.wantAtt || gotOK != tt.wantOK {
			t.Errorf("parseAttIDArg(%q) = (%q,%q,%v), want (%q,%q,%v)",
				tt.in, gotMsg, gotAtt, gotOK, tt.wantMsg, tt.wantAtt, tt.wantOK)
		}
	}
}

func TestByteCount(t *testing.T) {
	tests := []struct {
		n    int64
		want string
	}{
		{0, "0B"},
		{512, "512B"},
		{1024, "1.0KB"},
		{1536, "1.5KB"},
		{1048576, "1.0MB"},
	}
	for _, tt := range tests {
		if got := byteCount(tt.n); got != tt.want {
			t.Errorf("byteCount(%d) = %q, want %q", tt.n, got, tt.want)
		}
	}
}

func TestTruncate(t *testing.T) {
	if got := truncate("hello", 10); got != "hello" {
		t.Errorf("truncate short = %q, want hello", got)
	}
	if got := truncate("hello world", 5); got != "hell…" {
		t.Errorf("truncate = %q, want hell…", got)
	}
}

// --- emit / DTO seam ---

func TestEmitJSON(t *testing.T) {
	var buf bytes.Buffer
	s := &Shell{out: &buf, jsonOut: true}
	textCalled := false
	if err := s.emit(actionResult{Action: "trashed", Name: "old", ID: "m1"}, func() { textCalled = true }); err != nil {
		t.Fatal(err)
	}
	if textCalled {
		t.Error("text closure must not run in JSON mode")
	}
	want := `{"action":"trashed","name":"old","id":"m1"}` + "\n"
	if buf.String() != want {
		t.Errorf("emit JSON = %q, want %q", buf.String(), want)
	}
}

func TestEmitText(t *testing.T) {
	var buf bytes.Buffer
	s := &Shell{out: &buf, jsonOut: false}
	if err := s.emit(pwdResult{Path: "/INBOX"}, func() { buf.WriteString("text") }); err != nil {
		t.Fatal(err)
	}
	if buf.String() != "text" {
		t.Errorf("text mode should run the closure, got %q", buf.String())
	}
}

func TestEncodeErrorJSON(t *testing.T) {
	var buf bytes.Buffer
	encodeErrorJSON(&buf, errors.New("no such file or directory"))
	want := `{"error":"no such file or directory"}` + "\n"
	if buf.String() != want {
		t.Errorf("encodeErrorJSON = %q, want %q", buf.String(), want)
	}
}

func TestMessageEntryOmitsEmptyOptionalFields(t *testing.T) {
	var buf bytes.Buffer
	s := &Shell{out: &buf, jsonOut: true}
	// A message with no From/Date and no UNREAD label: those fields are omitted.
	m := hdr("m9", "t9", 0, []string{"INBOX"}, "Hi")
	if err := s.emit([]entry{messageEntry(m)}, func() {}); err != nil {
		t.Fatal(err)
	}
	got := buf.String()
	if !strings.Contains(got, `"kind":"message"`) {
		t.Errorf("message entry should carry kind=message: %s", got)
	}
	for _, absent := range []string{`"from"`, `"date"`, `"unread"`} {
		if strings.Contains(got, absent) {
			t.Errorf("expected %s omitted when empty/false: %s", absent, got)
		}
	}
	if !strings.Contains(got, `"threadId":"t9"`) {
		t.Errorf("message entry should carry threadId: %s", got)
	}
}

// --- command dispatch against the fake client ---

func TestCmdLsRootListsLabels(t *testing.T) {
	f := newFake()
	s, buf := newShell(f, false)
	if err := s.cmdLs(nil); err != nil {
		t.Fatal(err)
	}
	out := buf.String()
	if !strings.Contains(out, "INBOX/") || !strings.Contains(out, "Work/") {
		t.Errorf("root ls should list labels with trailing slash, got:\n%s", out)
	}
}

func TestCmdLsLabelListsMessages(t *testing.T) {
	f := newFake()
	s, buf := newShell(f, false)
	s.cwd = labelStack
	if err := s.cmdLs(nil); err != nil {
		t.Fatal(err)
	}
	out := buf.String()
	if !strings.Contains(out, "Hello") || !strings.Contains(out, "Invoice") {
		t.Errorf("label ls should list message names, got:\n%s", out)
	}
	if !strings.Contains(out, "*") {
		t.Errorf("unread message should be flagged with *, got:\n%s", out)
	}
}

func TestCmdCdRejectsMessageAsDirectory(t *testing.T) {
	f := newFake()
	s, _ := newShell(f, false)
	s.cwd = labelStack
	// "Hello" is a message under INBOX, not a label → cd must refuse.
	if err := s.cmdCd([]string{"Hello"}); err == nil {
		t.Error("cd into a message should fail (messages are leaves)")
	}
}

// rm safety: trashing a message by name (or id:) trashes exactly that one
// message via TrashMessage and NEVER calls TrashThread.
func TestCmdRmTrashesSingleMessageNotThread(t *testing.T) {
	f := newFake()
	s, _ := newShell(f, false)
	s.cwd = labelStack
	if err := s.cmdRm([]string{"Hello"}); err != nil {
		t.Fatal(err)
	}
	if len(f.trashedMessages) != 1 || f.trashedMessages[0] != "m1" {
		t.Errorf("rm should trash exactly message m1, got %v", f.trashedMessages)
	}
	if len(f.trashedThreads) != 0 {
		t.Errorf("rm of a message must NEVER trash a thread, got %v", f.trashedThreads)
	}
}

func TestCmdRmByIDTrashesSingleMessage(t *testing.T) {
	f := newFake()
	s, _ := newShell(f, false)
	if err := s.cmdRm([]string{"id:m2"}); err != nil {
		t.Fatal(err)
	}
	if len(f.trashedMessages) != 1 || f.trashedMessages[0] != "m2" {
		t.Errorf("rm id:m2 should trash m2, got %v", f.trashedMessages)
	}
	if len(f.trashedThreads) != 0 {
		t.Errorf("rm id:<msg> must not trash a thread, got %v", f.trashedThreads)
	}
}

// The whole-thread trash is reachable ONLY via the explicit id:thread: opt-in.
func TestCmdRmThreadOnlyViaExplicitOptIn(t *testing.T) {
	f := newFake()
	s, _ := newShell(f, false)
	if err := s.cmdRm([]string{"id:thread:t1"}); err != nil {
		t.Fatal(err)
	}
	if len(f.trashedThreads) != 1 || f.trashedThreads[0] != "t1" {
		t.Errorf("rm id:thread:t1 should trash thread t1, got %v", f.trashedThreads)
	}
	if len(f.trashedMessages) != 0 {
		t.Errorf("explicit thread trash should not trash individual messages, got %v", f.trashedMessages)
	}
}

// mkdir = create a Gmail user label (never a message-level mutation).
func TestCmdMkdirCreatesLabel(t *testing.T) {
	f := newFake()
	s, buf := newShell(f, false)
	if err := s.cmdMkdir([]string{"Receipts"}); err != nil {
		t.Fatal(err)
	}
	if len(f.createdLabels) != 1 || f.createdLabels[0] != "Receipts" {
		t.Errorf("mkdir should create label Receipts, got %v", f.createdLabels)
	}
	if f.modifyCalls != 0 {
		t.Errorf("mkdir must not modify message labels")
	}
	if !strings.Contains(buf.String(), "created label Receipts") {
		t.Errorf("mkdir output: %s", buf.String())
	}
}

// put = create a DRAFT only; it must never send.
func TestCmdPutCreatesDraftNeverSends(t *testing.T) {
	dir := t.TempDir()
	local := filepath.Join(dir, "msg.eml")
	if err := os.WriteFile(local, []byte("Subject: Hi\r\n\r\nbody"), 0o644); err != nil {
		t.Fatal(err)
	}
	f := newFake()
	s, buf := newShell(f, false)
	if err := s.cmdPut([]string{local}); err != nil {
		t.Fatal(err)
	}
	if f.createdDrafts != 1 {
		t.Errorf("put should create exactly one draft, got %d", f.createdDrafts)
	}
	out := buf.String()
	if !strings.Contains(out, "drafted") || !strings.Contains(out, "not sent") {
		t.Errorf("put output must say drafted/not sent, got: %s", out)
	}
}

// send/label/unlabel are deferred to v1.1: their handlers exist but must return a
// clear deferral notice and perform no mutation.
func TestDeferredVerbsAreStubbed(t *testing.T) {
	f := newFake()
	s, _ := newShell(f, false)
	for _, verb := range []struct {
		name string
		run  func([]string) error
	}{
		{"send", s.cmdSend},
		{"label", s.cmdLabel},
		{"unlabel", s.cmdUnlabel},
	} {
		err := verb.run([]string{"x", "y"})
		if err == nil || !strings.Contains(err.Error(), "deferred to v1.1") {
			t.Errorf("%s should return a 'deferred to v1.1' notice, got %v", verb.name, err)
		}
	}
	if f.createdDrafts != 0 || f.modifyCalls != 0 || len(f.trashedMessages) != 0 {
		t.Error("deferred verbs must not mutate anything")
	}
}

func TestCmdGetMessageWritesEml(t *testing.T) {
	dir := t.TempDir()
	f := newFake()
	s, _ := newShell(f, false)
	dest := filepath.Join(dir, "out.eml")
	if err := s.cmdGet([]string{"id:m1", dest}); err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(dest)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(b), "Subject: raw") {
		t.Errorf("get should write raw .eml bytes, got %q", b)
	}
}

func TestCmdGetAttachmentByID(t *testing.T) {
	dir := t.TempDir()
	// Give m1 an attachment.
	f := newFake()
	f.byID["m1"].Payload = &gmail.MessagePart{
		Parts: []*gmail.MessagePart{
			{Filename: "report.pdf", MimeType: "application/pdf",
				Body: &gmail.MessagePartBody{AttachmentId: "att1", Size: 16}},
		},
	}
	s, _ := newShell(f, false)
	dest := filepath.Join(dir, "report.pdf")
	if err := s.cmdGet([]string{"id:att:m1:att1", dest}); err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(dest)
	if err != nil {
		t.Fatal(err)
	}
	if string(b) != "ATTACHMENT-BYTES" {
		t.Errorf("attachment bytes = %q, want ATTACHMENT-BYTES", b)
	}
}

// --- resolveLocalDest / saveToFile (download plumbing, copied from gdrive-ftp) ---

func TestResolveLocalDest(t *testing.T) {
	dir := t.TempDir()
	if got := resolveLocalDest("", "remote.eml"); got != "remote.eml" {
		t.Errorf("empty local: got %q, want remote.eml", got)
	}
	if got := resolveLocalDest("named.bin", "remote.eml"); got != "named.bin" {
		t.Errorf("named local: got %q, want named.bin", got)
	}
	if got, want := resolveLocalDest(dir, "remote.eml"), filepath.Join(dir, "remote.eml"); got != want {
		t.Errorf("existing dir: got %q, want %q", got, want)
	}
	if got, want := resolveLocalDest(dir+"/", "remote.eml"), filepath.Join(dir, "remote.eml"); got != want {
		t.Errorf("trailing slash: got %q, want %q", got, want)
	}
}

func TestSaveToFileSuccess(t *testing.T) {
	dest := filepath.Join(t.TempDir(), "out.txt")
	got, n, err := saveToFile(dest, func(w io.Writer) (int64, error) {
		m, err := io.WriteString(w, "hello")
		return int64(m), err
	})
	if err != nil {
		t.Fatalf("saveToFile: %v", err)
	}
	if got != dest || n != 5 {
		t.Fatalf("saveToFile = (%q, %d), want (%q, 5)", got, n, dest)
	}
	b, err := os.ReadFile(dest)
	if err != nil || string(b) != "hello" {
		t.Fatalf("file content = %q (err %v), want hello", b, err)
	}
}

func TestSaveToFilePreservesExistingOnError(t *testing.T) {
	dest := filepath.Join(t.TempDir(), "keep.txt")
	if err := os.WriteFile(dest, []byte("original"), 0o644); err != nil {
		t.Fatal(err)
	}
	_, _, err := saveToFile(dest, func(w io.Writer) (int64, error) {
		io.WriteString(w, "partial")
		return 7, errors.New("boom")
	})
	if err == nil {
		t.Fatal("expected error from failing writer")
	}
	b, _ := os.ReadFile(dest)
	if string(b) != "original" {
		t.Fatalf("existing file clobbered: got %q, want original", b)
	}
	entries, _ := os.ReadDir(filepath.Dir(dest))
	if len(entries) != 1 {
		t.Fatalf("leftover temp files: %d entries, want 1", len(entries))
	}
}
