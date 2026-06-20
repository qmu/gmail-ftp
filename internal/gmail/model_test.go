package gmail

import (
	"reflect"
	"testing"
	"time"

	gmail "google.golang.org/api/gmail/v1"
)

// msg builds a synthetic message with the given internalDate (ms), label IDs,
// and headers, for name/header/unread tests without touching the API.
func msg(internalMillis int64, labels []string, headers map[string]string) *gmail.Message {
	var hs []*gmail.MessagePartHeader
	for k, v := range headers {
		hs = append(hs, &gmail.MessagePartHeader{Name: k, Value: v})
	}
	return &gmail.Message{
		InternalDate: internalMillis,
		LabelIds:     labels,
		Payload:      &gmail.MessagePart{Headers: hs},
	}
}

func TestMessageName(t *testing.T) {
	// 2026-06-18 12:00:00 UTC in epoch ms.
	d := time.Date(2026, 6, 18, 12, 0, 0, 0, time.UTC).UnixMilli()
	datePrefix := time.UnixMilli(d).Local().Format("2006-01-02")
	tests := []struct {
		name string
		in   *gmail.Message
		want string
	}{
		{"date + subject", msg(d, nil, map[string]string{"Subject": "Quarterly report"}),
			datePrefix + " Quarterly report"},
		{"empty subject", msg(d, nil, map[string]string{"Subject": ""}),
			datePrefix + " (no subject)"},
		{"missing subject header", msg(d, nil, nil),
			datePrefix + " (no subject)"},
		{"no date drops prefix", msg(0, nil, map[string]string{"Subject": "Hello"}),
			"Hello"},
		{"subject with slash is sanitized", msg(d, nil, map[string]string{"Subject": "a/b\nc"}),
			datePrefix + " a b c"},
		{"collapses whitespace", msg(d, nil, map[string]string{"Subject": "  spaced   out  "}),
			datePrefix + " spaced out"},
	}
	for _, tt := range tests {
		if got := MessageName(tt.in); got != tt.want {
			t.Errorf("%s: MessageName = %q, want %q", tt.name, got, tt.want)
		}
	}
}

func TestHeaderValue(t *testing.T) {
	m := msg(0, nil, map[string]string{"Subject": "Hi", "From": "a@x.test"})
	if got := headerValue(m, "subject"); got != "Hi" { // case-insensitive
		t.Errorf("headerValue(subject) = %q, want Hi", got)
	}
	if got := headerValue(m, "From"); got != "a@x.test" {
		t.Errorf("headerValue(From) = %q, want a@x.test", got)
	}
	if got := headerValue(m, "Cc"); got != "" {
		t.Errorf("headerValue(absent) = %q, want empty", got)
	}
	if got := headerValue(nil, "Subject"); got != "" {
		t.Errorf("headerValue(nil) = %q, want empty", got)
	}
}

func TestIsUnread(t *testing.T) {
	if !isUnread(msg(0, []string{"INBOX", "UNREAD"}, nil)) {
		t.Error("expected unread when UNREAD label present")
	}
	if isUnread(msg(0, []string{"INBOX"}, nil)) {
		t.Error("expected read when UNREAD label absent")
	}
	if isUnread(nil) {
		t.Error("nil message should not be unread")
	}
}

func TestDecodePartBase64URL(t *testing.T) {
	// "Hello, world" in base64url, unpadded.
	body := &gmail.MessagePartBody{Data: "SGVsbG8sIHdvcmxk"}
	part := &gmail.MessagePart{Body: body}
	got, err := decodePart(part)
	if err != nil {
		t.Fatalf("decodePart: %v", err)
	}
	if string(got) != "Hello, world" {
		t.Errorf("decodePart = %q, want %q", got, "Hello, world")
	}
	// Empty / attachment-only body decodes to nil without error.
	if b, err := decodePart(&gmail.MessagePart{}); err != nil || b != nil {
		t.Errorf("empty body: got (%v, %v), want (nil, nil)", b, err)
	}
}

func TestWalkPartsNested(t *testing.T) {
	payload := &gmail.MessagePart{
		MimeType: "multipart/mixed",
		Parts: []*gmail.MessagePart{
			{MimeType: "text/plain", Body: &gmail.MessagePartBody{Data: "aGk"}},
			{
				MimeType: "multipart/related",
				Parts: []*gmail.MessagePart{
					{Filename: "img.png", MimeType: "image/png",
						Body: &gmail.MessagePartBody{AttachmentId: "att1", Size: 1234}},
				},
			},
			{Filename: "report.pdf", MimeType: "application/pdf",
				Body: &gmail.MessagePartBody{AttachmentId: "att2", Size: 5678}},
		},
	}
	atts := Attachments(&gmail.Message{Payload: payload})
	want := []Attachment{
		{Filename: "img.png", AttachmentID: "att1", MimeType: "image/png", Size: 1234},
		{Filename: "report.pdf", AttachmentID: "att2", MimeType: "application/pdf", Size: 5678},
	}
	if !reflect.DeepEqual(atts, want) {
		t.Errorf("Attachments = %#v, want %#v", atts, want)
	}
	// A part with a filename but no attachment ID (e.g. inline body) is skipped.
	none := Attachments(&gmail.Message{Payload: &gmail.MessagePart{
		Filename: "noid.txt", Body: &gmail.MessagePartBody{Data: "x"}},
	})
	if len(none) != 0 {
		t.Errorf("attachment with no id should be skipped, got %#v", none)
	}
}

func TestTextBody(t *testing.T) {
	payload := &gmail.MessagePart{
		MimeType: "multipart/alternative",
		Parts: []*gmail.MessagePart{
			{MimeType: "text/html", Body: &gmail.MessagePartBody{Data: "PGI-aGk8L2I-"}},
			{MimeType: "text/plain; charset=UTF-8", Body: &gmail.MessagePartBody{Data: "SGVsbG8"}},
		},
	}
	if got := string(textBody(payload)); got != "Hello" {
		t.Errorf("textBody preferred plain = %q, want Hello", got)
	}
	// HTML-only falls back to the HTML body.
	htmlOnly := &gmail.MessagePart{MimeType: "text/html", Body: &gmail.MessagePartBody{Data: "PGI-aGk8L2I-"}}
	if got := string(textBody(htmlOnly)); got != "<b>hi</b>" {
		t.Errorf("textBody html fallback = %q, want <b>hi</b>", got)
	}
}

func TestSortLabels(t *testing.T) {
	in := []Label{
		{ID: "Label_5", Name: "zeta", System: false},
		{ID: "SENT", Name: "SENT", System: true},
		{ID: "INBOX", Name: "INBOX", System: true},
		{ID: "Label_1", Name: "alpha", System: false},
		{ID: "TRASH", Name: "TRASH", System: true},
	}
	sortLabels(in)
	var order []string
	for _, l := range in {
		order = append(order, l.ID)
	}
	want := []string{"INBOX", "SENT", "TRASH", "Label_1", "Label_5"}
	if !reflect.DeepEqual(order, want) {
		t.Errorf("sortLabels order = %v, want %v", order, want)
	}
}

func TestNormalizeLabelName(t *testing.T) {
	tests := map[string]string{
		"  Work  ":         "Work",
		"Work/Receipts":    "Work/Receipts",
		"Work//Receipts":   "Work/Receipts",
		" Work / Receipts": "Work/Receipts",
		"///":              "",
	}
	for in, want := range tests {
		if got := normalizeLabelName(in); got != want {
			t.Errorf("normalizeLabelName(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestNameContains(t *testing.T) {
	tests := []struct {
		name, pattern string
		want          bool
	}{
		{"2026-06-18 Quarterly Report", "report", true}, // case-insensitive
		{"Invoice 42", "INVOICE", true},
		{"Hello", "report", false},
		{"anything", "", true}, // empty pattern matches anything
	}
	for _, tt := range tests {
		if got := nameContains(tt.name, tt.pattern); got != tt.want {
			t.Errorf("nameContains(%q,%q) = %v, want %v", tt.name, tt.pattern, got, tt.want)
		}
	}
}

func TestRoundTripBase64URL(t *testing.T) {
	orig := []byte("Subject: Test\r\n\r\nbody with binary \x00\xff bytes")
	enc := encodeRaw(orig)
	got, err := decodeRaw(enc)
	if err != nil {
		t.Fatalf("decodeRaw: %v", err)
	}
	if !reflect.DeepEqual(got, orig) {
		t.Errorf("round-trip mismatch: got %q, want %q", got, orig)
	}
}
