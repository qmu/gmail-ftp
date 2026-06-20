// Package gmail is a thin, FTP-flavored wrapper over the Gmail v1 API. It
// exposes the handful of operations the shell needs — listing labels, listing
// and searching messages, fetching a message/attachment, creating a draft,
// trashing a message, and creating a label — re-expressed in email terms while
// quarantining the Gmail SDK behind one package. Thread access (an internal
// batching detail for id:thread: addressing) lives here, one layer below the
// shell, and is never a navigation tier.
package gmail

import (
	"context"
	"errors"
	"fmt"
	"net/http"

	"google.golang.org/api/googleapi"
	"google.golang.org/api/option"

	gmail "google.golang.org/api/gmail/v1"
)

// user is the Gmail special value for "the authenticated user".
const user = "me"

// defaultPageCap bounds the first listing of a label so the first ls on a large
// mailbox is a fast partial list rather than a stall. Callers learn there is
// more via Truncated on the result.
const defaultPageCap = 50

// metadataHeaders is the restricted header set fetched for a listing row, so a
// metadata get stays cheap (no full body).
var metadataHeaders = []string{"Subject", "From", "Date"}

// ErrNotFound is returned when a message, label, or attachment does not exist.
var ErrNotFound = errors.New("no such file or directory")

// ErrAmbiguous is returned when a synthesized message name matches more than one
// message and an operation cannot safely pick one.
var ErrAmbiguous = errors.New("ambiguous name (multiple matches); address by id: to disambiguate")

// Client wraps an authenticated Gmail service.
type Client struct {
	srv *gmail.Service
}

// New builds a Client from an authorized HTTP client.
func New(ctx context.Context, hc *http.Client) (*Client, error) {
	srv, err := gmail.NewService(ctx, option.WithHTTPClient(hc))
	if err != nil {
		return nil, fmt.Errorf("creating gmail service: %w", err)
	}
	return &Client{srv: srv}, nil
}

// notFound maps a Gmail 404 to ErrNotFound, matching the name-lookup path.
func notFound(err error) error {
	var ge *googleapi.Error
	if errors.As(err, &ge) && ge.Code == 404 {
		return ErrNotFound
	}
	return err
}

// ListLabels returns the user's labels as owned Refs, system labels (INBOX,
// SENT, …) first and user labels after, mirroring how gdrive-ftp lists drives
// at the virtual root.
func (c *Client) ListLabels(ctx context.Context) ([]Label, error) {
	resp, err := c.srv.Users.Labels.List(user).Context(ctx).Do()
	if err != nil {
		return nil, err
	}
	out := make([]Label, 0, len(resp.Labels))
	for _, l := range resp.Labels {
		out = append(out, toLabel(l))
	}
	sortLabels(out)
	return out, nil
}

// MessageList is the result of listing or searching messages: the per-row
// metadata messages plus whether the result was capped (more rows exist).
type MessageList struct {
	Messages  []*gmail.Message
	Truncated bool
}

// ListMessages lists the messages under labelID (optionally narrowed by a Gmail
// query), then fetches each one's metadata (Subject/From/Date only) so the shell
// can render a human-readable listing. The result is capped at defaultPageCap so
// the first listing of a large label stays fast; Truncated reports when more
// rows exist. An empty labelID lists across all labels (used by search).
func (c *Client) ListMessages(ctx context.Context, labelID, query string) (*MessageList, error) {
	call := c.srv.Users.Messages.List(user).MaxResults(int64(defaultPageCap))
	if labelID != "" {
		call = call.LabelIds(labelID)
	}
	if query != "" {
		call = call.Q(query)
	}
	resp, err := call.Context(ctx).Do()
	if err != nil {
		return nil, err
	}
	out := &MessageList{Truncated: resp.NextPageToken != ""}
	for _, m := range resp.Messages {
		full, err := c.getMetadata(ctx, m.Id)
		if err != nil {
			return nil, err
		}
		out.Messages = append(out.Messages, full)
	}
	return out, nil
}

// Search runs a raw Gmail search query (the from:/subject:/is:unread syntax)
// across the whole mailbox and returns the matching messages' metadata. It is
// the direct analogue of Drive's name-contains search.
func (c *Client) Search(ctx context.Context, query string) (*MessageList, error) {
	return c.ListMessages(ctx, "", query)
}

// getMetadata fetches one message with the restricted metadata header set — the
// cheap per-row call used for listings (never a full body).
func (c *Client) getMetadata(ctx context.Context, id string) (*gmail.Message, error) {
	m, err := c.srv.Users.Messages.Get(user, id).
		Format("metadata").
		MetadataHeaders(metadataHeaders...).
		Context(ctx).
		Do()
	if err != nil {
		return nil, notFound(err)
	}
	return m, nil
}

// GetMessage fetches a single message in the requested format ("metadata",
// "full", or "raw"). "full" backs attachment listing and the .txt export;
// "metadata" backs a single-row ls; "raw" backs the .eml download.
func (c *Client) GetMessage(ctx context.Context, id, format string) (*gmail.Message, error) {
	call := c.srv.Users.Messages.Get(user, id)
	if format != "" {
		call = call.Format(format)
	}
	if format == "metadata" {
		call = call.MetadataHeaders(metadataHeaders...)
	}
	m, err := call.Context(ctx).Do()
	if err != nil {
		return nil, notFound(err)
	}
	return m, nil
}

// GetRawMessage returns a message's raw RFC 5322 bytes (the .eml form),
// decoding the base64url "raw" field the API returns.
func (c *Client) GetRawMessage(ctx context.Context, id string) ([]byte, error) {
	m, err := c.GetMessage(ctx, id, "raw")
	if err != nil {
		return nil, err
	}
	return decodeRaw(m.Raw)
}

// GetAttachment fetches one attachment's bytes by message and attachment ID,
// decoding the base64url body the API returns.
func (c *Client) GetAttachment(ctx context.Context, msgID, attID string) ([]byte, error) {
	body, err := c.srv.Users.Messages.Attachments.Get(user, msgID, attID).Context(ctx).Do()
	if err != nil {
		return nil, notFound(err)
	}
	return decodeRaw(body.Data)
}

// GetThread fetches a whole thread (all its messages, full format). It backs the
// id:thread:<id> opt-in addressing and .mbox export only — it is an internal
// batching detail, never a navigation tier.
func (c *Client) GetThread(ctx context.Context, id string) (*gmail.Thread, error) {
	t, err := c.srv.Users.Threads.Get(user, id).Format("full").Context(ctx).Do()
	if err != nil {
		return nil, notFound(err)
	}
	return t, nil
}

// CreateDraft creates a draft from a raw RFC 5322 message. It NEVER sends — the
// (deferred) send verb is the only path that sends mail. Returns the draft and
// its underlying message.
func (c *Client) CreateDraft(ctx context.Context, raw []byte) (*gmail.Draft, error) {
	d := &gmail.Draft{
		Message: &gmail.Message{Raw: encodeRaw(raw)},
	}
	created, err := c.srv.Users.Drafts.Create(user, d).Context(ctx).Do()
	if err != nil {
		return nil, err
	}
	return created, nil
}

// TrashMessage moves a single message to TRASH (the default, reversible rm
// target). It never trashes a whole thread.
func (c *Client) TrashMessage(ctx context.Context, id string) (*gmail.Message, error) {
	m, err := c.srv.Users.Messages.Trash(user, id).Context(ctx).Do()
	if err != nil {
		return nil, notFound(err)
	}
	return m, nil
}

// TrashThread moves a whole thread to TRASH. It is reachable ONLY via the
// explicit rm id:thread:<id> opt-in, never as an implicit blast radius of
// trashing one message.
func (c *Client) TrashThread(ctx context.Context, id string) (*gmail.Thread, error) {
	t, err := c.srv.Users.Threads.Trash(user, id).Context(ctx).Do()
	if err != nil {
		return nil, notFound(err)
	}
	return t, nil
}

// ModifyLabels adds and/or removes labels on a message. It backs the (deferred)
// label/unlabel verbs.
func (c *Client) ModifyLabels(ctx context.Context, id string, add, remove []string) (*gmail.Message, error) {
	req := &gmail.ModifyMessageRequest{AddLabelIds: add, RemoveLabelIds: remove}
	m, err := c.srv.Users.Messages.Modify(user, id, req).Context(ctx).Do()
	if err != nil {
		return nil, notFound(err)
	}
	return m, nil
}

// CreateLabel creates a user label (the mkdir analogue). The name is normalized
// (trimmed, collapsed nested-label separators) before creation.
func (c *Client) CreateLabel(ctx context.Context, name string) (*gmail.Label, error) {
	name = normalizeLabelName(name)
	if name == "" {
		return nil, fmt.Errorf("invalid label name")
	}
	l := &gmail.Label{
		Name:                  name,
		LabelListVisibility:   "labelShow",
		MessageListVisibility: "show",
	}
	return c.srv.Users.Labels.Create(user, l).Context(ctx).Do()
}

// DeleteLabel deletes a user label by ID.
func (c *Client) DeleteLabel(ctx context.Context, id string) error {
	return notFound(c.srv.Users.Labels.Delete(user, id).Context(ctx).Do())
}

// FindLabel resolves a label by its display name (exact, case-sensitive),
// refusing to guess when several labels share the name.
func (c *Client) FindLabel(ctx context.Context, name string) (Label, error) {
	labels, err := c.ListLabels(ctx)
	if err != nil {
		return Label{}, err
	}
	var match []Label
	for _, l := range labels {
		if l.Name == name {
			match = append(match, l)
		}
	}
	switch len(match) {
	case 0:
		return Label{}, ErrNotFound
	case 1:
		return match[0], nil
	default:
		return Label{}, ErrAmbiguous
	}
}

// FindMessageByName resolves a synthesized message name to a single message
// within labelID, refusing to guess when the name is shared. The lookup fetches
// the label's listing and matches MessageName; id: addressing is the
// unambiguous escape hatch when names collide.
func (c *Client) FindMessageByName(ctx context.Context, labelID, name string) (*gmail.Message, error) {
	list, err := c.ListMessages(ctx, labelID, "")
	if err != nil {
		return nil, err
	}
	var match []*gmail.Message
	for _, m := range list.Messages {
		if MessageName(m) == name {
			match = append(match, m)
		}
	}
	switch len(match) {
	case 0:
		return nil, ErrNotFound
	case 1:
		return match[0], nil
	default:
		return nil, ErrAmbiguous
	}
}

// decodeRaw decodes a base64url string (the form Gmail uses for raw messages and
// attachment bodies). It tolerates both padded and unpadded input and the
// standard alphabet as a fallback, since the API has historically used both.
func decodeRaw(s string) ([]byte, error) {
	if s == "" {
		return nil, nil
	}
	if b, err := base64URLDecode(s); err == nil {
		return b, nil
	}
	return base64StdDecode(s)
}

// encodeRaw encodes raw RFC 5322 bytes as the base64url form Gmail expects for a
// message's Raw field.
func encodeRaw(b []byte) string {
	return base64URLEncode(b)
}
