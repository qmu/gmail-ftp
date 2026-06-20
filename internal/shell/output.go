package shell

import (
	"encoding/json"
	"io"

	gmailpkg "gmail-ftp/internal/gmail"

	gmail "google.golang.org/api/gmail/v1"
)

// This file holds the machine-readable output layer: owned DTOs that the
// commands serialize under the global -json flag, plus the emit() render seam.
// The Gmail SDK's *gmail.Message / *gmail.Label is never marshaled directly — it
// is translated into these owned types so the public JSON contract stays
// decoupled from the vendor struct shape and field names.

// entry is one label, message, or attachment in machine-readable form. A label
// entry sets Name/ID/Kind. A message row sets From/Subject/Date/Unread/ThreadID
// (and Size). An attachment leaf sets Name/ID/MimeType/Size. Path is set only by
// `find` (where a match's label location matters); other commands leave it empty
// and it is omitted.
type entry struct {
	Path     string `json:"path,omitempty"`
	Name     string `json:"name"`
	ID       string `json:"id"`
	Kind     string `json:"kind"`
	From     string `json:"from,omitempty"`
	Subject  string `json:"subject,omitempty"`
	Date     string `json:"date,omitempty"`
	Unread   bool   `json:"unread,omitempty"`
	MimeType string `json:"mimeType,omitempty"`
	Size     int64  `json:"size,omitempty"`
	ThreadID string `json:"threadId,omitempty"`
}

// actionResult is the JSON result of a mutating/transfer command (get, put, rm,
// mkdir, and the deferred label/unlabel/send). Fields irrelevant to a given
// action are omitted.
type actionResult struct {
	Action   string   `json:"action"`
	Name     string   `json:"name"`
	ID       string   `json:"id,omitempty"`
	Dest     string   `json:"dest,omitempty"`
	ThreadID string   `json:"threadId,omitempty"`
	Size     int64    `json:"size,omitempty"`
	Labels   []string `json:"labels,omitempty"`
}

// pwdResult is the JSON result of pwd.
type pwdResult struct {
	Path string `json:"path"`
}

// errorResult is the JSON error envelope emitted (to stderr in one-shot mode)
// when a command fails.
type errorResult struct {
	Error string `json:"error"`
}

// labelEntry translates a label into the owned DTO (a navigable container).
func labelEntry(l gmailpkg.Label) entry {
	return entry{Name: l.Name, ID: l.ID, Kind: string(gmailpkg.KindLabel)}
}

// messageEntry translates a Gmail message (fetched with at least the metadata
// headers) into the owned message-row DTO, synthesizing its display name.
func messageEntry(m *gmail.Message) entry {
	return entry{
		Name:     gmailpkg.MessageName(m),
		ID:       m.Id,
		Kind:     string(gmailpkg.KindMessage),
		From:     gmailpkg.Header(m, "From"),
		Subject:  gmailpkg.Header(m, "Subject"),
		Date:     gmailpkg.Header(m, "Date"),
		Unread:   gmailpkg.Unread(m),
		Size:     m.SizeEstimate,
		ThreadID: m.ThreadId,
	}
}

// attachmentEntry translates an attachment leaf into the owned DTO. The ID is
// the id:att:<msgID>:<attID> addressing token so a -json consumer can fetch it.
func attachmentEntry(a gmailpkg.Attachment, addr string) entry {
	return entry{
		Name:     a.Filename,
		ID:       addr,
		Kind:     "attachment",
		MimeType: a.MimeType,
		Size:     a.Size,
	}
}

// emit renders a command's result. In JSON mode it encodes v (compact, one line,
// newline-terminated, HTML escaping off) to the shell's output writer; otherwise
// it runs the text closure that prints the human-readable form.
func (s *Shell) emit(v any, text func()) error {
	if !s.jsonOut {
		text()
		return nil
	}
	enc := json.NewEncoder(s.out)
	enc.SetEscapeHTML(false)
	return enc.Encode(v)
}

// encodeErrorJSON writes an {"error": …} object to w. Used for one-shot JSON
// error output on stderr (exit code is owned by the caller in main).
func encodeErrorJSON(w io.Writer, err error) {
	enc := json.NewEncoder(w)
	enc.SetEscapeHTML(false)
	_ = enc.Encode(errorResult{Error: err.Error()})
}

// EncodeErrorJSON is the exported entry point for the one-shot error path in
// main: it serializes err as a JSON {"error": …} object to w.
func EncodeErrorJSON(w io.Writer, err error) { encodeErrorJSON(w, err) }
