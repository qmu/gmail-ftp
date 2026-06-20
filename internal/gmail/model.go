package gmail

import (
	"encoding/base64"
	"sort"
	"strings"
	"time"

	gmail "google.golang.org/api/gmail/v1"
)

// Kind classifies what a Ref points at in the 2-level navigation model
// (root → label → message). Attachments are leaves inside a message and are
// addressed by id:, not by a Ref kind; threads are never a navigation tier.
type Kind string

const (
	KindLabel   Kind = "label"   // a navigable container under the virtual root
	KindMessage Kind = "message" // the canonical leaf under a label
)

// Ref is a lightweight pointer to a label or a message used to build the
// working-directory path without re-querying the API. For a message, ThreadID
// carries the conversation it belongs to (the only surfacing of the thread at
// the shell boundary); it is a field, never a navigable frame.
type Ref struct {
	ID       string
	Name     string
	Kind     Kind
	ThreadID string
}

// systemLabelOrder gives the canonical sort position of Gmail's well-known
// system labels so a listing of the virtual root shows INBOX first, etc. Any
// label not in the map sorts after all listed system labels, alphabetically.
var systemLabelOrder = map[string]int{
	"INBOX":               0,
	"STARRED":             1,
	"IMPORTANT":           2,
	"UNREAD":              3,
	"SENT":                4,
	"DRAFT":               5,
	"SPAM":                6,
	"TRASH":               7,
	"CATEGORY_PERSONAL":   8,
	"CATEGORY_SOCIAL":     9,
	"CATEGORY_PROMOTIONS": 10,
	"CATEGORY_UPDATES":    11,
	"CATEGORY_FORUMS":     12,
}

// isSystemLabel reports whether a label ID is one of Gmail's built-in system
// labels (which Gmail marks with Type "system").
func isSystemLabel(l *gmail.Label) bool {
	return l != nil && l.Type == "system"
}

// sortLabels orders labels with system labels first (in systemLabelOrder), then
// user labels alphabetically by name. Sorting is stable and in place.
func sortLabels(labels []Label) {
	sort.SliceStable(labels, func(i, j int) bool {
		li, lj := labels[i], labels[j]
		oi, iok := systemLabelOrder[li.ID]
		oj, jok := systemLabelOrder[lj.ID]
		switch {
		case li.System && lj.System:
			// Both system: known ones by fixed order, unknown system labels after.
			if iok && jok {
				return oi < oj
			}
			if iok != jok {
				return iok
			}
			return li.Name < lj.Name
		case li.System != lj.System:
			return li.System // system labels precede user labels
		default:
			return strings.ToLower(li.Name) < strings.ToLower(lj.Name)
		}
	})
}

// Label is an owned label DTO, decoupled from the vendor *gmail.Label.
type Label struct {
	ID     string
	Name   string
	System bool
}

// toLabel translates a vendor label into the owned DTO.
func toLabel(l *gmail.Label) Label {
	return Label{ID: l.Id, Name: l.Name, System: isSystemLabel(l)}
}

// headerValue returns the value of the named header (case-insensitive) from a
// message's payload, or "" if absent.
func headerValue(msg *gmail.Message, name string) string {
	if msg == nil || msg.Payload == nil {
		return ""
	}
	for _, h := range msg.Payload.Headers {
		if strings.EqualFold(h.Name, name) {
			return h.Value
		}
	}
	return ""
}

// isUnread reports whether a message still carries the UNREAD label.
func isUnread(msg *gmail.Message) bool {
	if msg == nil {
		return false
	}
	for _, id := range msg.LabelIds {
		if id == "UNREAD" {
			return true
		}
	}
	return false
}

// messageDate returns the message's received time, derived from internalDate
// (epoch milliseconds). A zero internalDate yields the zero time.
func messageDate(msg *gmail.Message) time.Time {
	if msg == nil || msg.InternalDate == 0 {
		return time.Time{}
	}
	return time.UnixMilli(msg.InternalDate)
}

// slugify trims a subject into a single-line, filesystem- and Tab-friendly
// fragment: it collapses internal whitespace to single spaces and drops path
// separators and control characters so a synthesized name never contains a "/"
// (which would break path parsing) or a newline.
func slugify(s string) string {
	s = strings.Map(func(r rune) rune {
		switch {
		case r == '/' || r == '\\':
			return ' '
		case r == '\n' || r == '\r' || r == '\t':
			return ' '
		case r < 0x20:
			return -1
		default:
			return r
		}
	}, s)
	return strings.Join(strings.Fields(s), " ")
}

// MessageName synthesizes a stable, human- and Tab-friendly display name for a
// message: a date prefix from internalDate plus the (slugified) Subject, e.g.
// "2026-06-18 Quarterly report". An empty subject renders as "(no subject)". A
// missing date drops the prefix. Names can collide (subjects are non-unique), so
// callers key navigation off message IDs (id:) and use this only for display.
func MessageName(msg *gmail.Message) string {
	subject := slugify(headerValue(msg, "Subject"))
	if subject == "" {
		subject = "(no subject)"
	}
	if d := messageDate(msg); !d.IsZero() {
		return d.Local().Format("2006-01-02") + " " + subject
	}
	return subject
}

// base64URLDecode/base64URLEncode handle the unpadded base64url encoding Gmail
// uses for raw messages, attachment bodies, and message-part data.
func base64URLDecode(s string) ([]byte, error) {
	return base64.URLEncoding.WithPadding(base64.NoPadding).DecodeString(strings.TrimRight(s, "="))
}

func base64URLEncode(b []byte) string {
	return base64.URLEncoding.WithPadding(base64.NoPadding).EncodeToString(b)
}

// base64StdDecode is the standard-alphabet fallback for inputs the API may emit
// with the legacy padded standard encoding.
func base64StdDecode(s string) ([]byte, error) {
	return base64.StdEncoding.DecodeString(s)
}

// decodePart decodes a message part's body, which the Gmail API returns as
// base64url (RFC 4648 URL alphabet, no padding requirement). It returns the raw
// bytes, or nil for an empty/attachment-only body.
func decodePart(part *gmail.MessagePart) ([]byte, error) {
	if part == nil || part.Body == nil || part.Body.Data == "" {
		return nil, nil
	}
	return base64URLDecode(part.Body.Data)
}

// Header returns a message header value by name (case-insensitive), or "". It is
// the exported accessor the shell's output layer uses to build a row.
func Header(msg *gmail.Message, name string) string { return headerValue(msg, name) }

// Unread reports whether a message carries the UNREAD label (exported accessor).
func Unread(msg *gmail.Message) bool { return isUnread(msg) }

// Date returns a message's received time from internalDate (exported accessor).
func Date(msg *gmail.Message) time.Time { return messageDate(msg) }

// Attachment describes one attachment leaf inside a message: the filename Gmail
// reports, the attachment ID needed to fetch its bytes, its declared MIME type,
// and its size estimate.
type Attachment struct {
	Filename     string
	AttachmentID string
	MimeType     string
	Size         int64
}

// walkParts recursively collects every attachment (a part with a filename and an
// attachment ID) from a message payload, depth-first. Nested multipart bodies
// are traversed in full.
func walkParts(part *gmail.MessagePart) []Attachment {
	if part == nil {
		return nil
	}
	var out []Attachment
	if part.Filename != "" && part.Body != nil && part.Body.AttachmentId != "" {
		out = append(out, Attachment{
			Filename:     part.Filename,
			AttachmentID: part.Body.AttachmentId,
			MimeType:     part.MimeType,
			Size:         part.Body.Size,
		})
	}
	for _, p := range part.Parts {
		out = append(out, walkParts(p)...)
	}
	return out
}

// Attachments returns every attachment leaf in a message.
func Attachments(msg *gmail.Message) []Attachment {
	if msg == nil {
		return nil
	}
	return walkParts(msg.Payload)
}

// textBody returns the readable body of a message: the first text/plain part
// in the tree, or — only when no plain part exists anywhere — the first
// text/html part, decoded from base64url. It is the basis of the readable .txt
// export. The two-pass search avoids returning an HTML alternative when a plain
// alternative is present later in the same multipart.
func textBody(part *gmail.MessagePart) []byte {
	if b := bodyOfType(part, "text/plain"); b != nil {
		return b
	}
	return bodyOfType(part, "text/html")
}

// bodyOfType walks the part tree depth-first and returns the decoded body of the
// first part whose MIME type has the given prefix.
func bodyOfType(part *gmail.MessagePart, mimePrefix string) []byte {
	if part == nil {
		return nil
	}
	if strings.HasPrefix(part.MimeType, mimePrefix) {
		if b, err := decodePart(part); err == nil && len(b) > 0 {
			return b
		}
	}
	for _, p := range part.Parts {
		if b := bodyOfType(p, mimePrefix); b != nil {
			return b
		}
	}
	return nil
}

// RenderText builds a readable .txt export of a message: a small header block
// (From/To/Date/Subject) followed by the decoded text body. It is the email
// analogue of gdrive-ftp's "export a native doc to a readable form".
func RenderText(msg *gmail.Message) []byte {
	var b strings.Builder
	for _, h := range []string{"From", "To", "Cc", "Date", "Subject"} {
		if v := headerValue(msg, h); v != "" {
			b.WriteString(h)
			b.WriteString(": ")
			b.WriteString(v)
			b.WriteByte('\n')
		}
	}
	b.WriteByte('\n')
	if msg != nil {
		b.Write(textBody(msg.Payload))
	}
	return []byte(b.String())
}

// nameContains reports whether name contains pattern as a case-insensitive
// substring — the exact semantics find promises over a label's rows.
func nameContains(name, pattern string) bool {
	return strings.Contains(strings.ToLower(name), strings.ToLower(pattern))
}

// NameContains reports whether a synthesized message name contains pattern as a
// case-insensitive substring (exported for the shell's find verb).
func NameContains(name, pattern string) bool { return nameContains(name, pattern) }

// normalizeLabelName cleans a user-supplied label name: it trims surrounding
// whitespace and collapses runs of "/" (Gmail's nested-label separator) so a
// stray "Work//Receipts" or " Work " does not create a surprising label.
func normalizeLabelName(name string) string {
	parts := strings.Split(name, "/")
	cleaned := make([]string, 0, len(parts))
	for _, p := range parts {
		if p = strings.TrimSpace(p); p != "" {
			cleaned = append(cleaned, p)
		}
	}
	return strings.Join(cleaned, "/")
}
