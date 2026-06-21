package gmail

import (
	"encoding/base64"
	"regexp"
	"strings"
	"testing"
)

// boundaryOf extracts the multipart boundary token declared in a raw message's
// top-level Content-Type header.
func boundaryOf(t *testing.T, raw string) string {
	t.Helper()
	m := regexp.MustCompile(`boundary="([^"]+)"`).FindStringSubmatch(raw)
	if m == nil {
		t.Fatalf("no boundary in:\n%s", raw)
	}
	return m[1]
}

func TestBuildMIMEPlainNoAttachments(t *testing.T) {
	raw := string(BuildMIME(
		MIMEHeaders{To: "a@b.test", Subject: "Hi"},
		[]byte("hello body"),
		nil,
	))
	for _, want := range []string{
		"MIME-Version: 1.0\r\n",
		"To: a@b.test\r\n",
		"Subject: Hi\r\n",
		"Content-Type: text/plain; charset=\"UTF-8\"\r\n",
		"hello body",
	} {
		if !strings.Contains(raw, want) {
			t.Errorf("plain MIME missing %q:\n%s", want, raw)
		}
	}
	if strings.Contains(raw, "multipart") {
		t.Errorf("no-attachment build should not be multipart:\n%s", raw)
	}
	// Cc is omitted when empty.
	if strings.Contains(raw, "Cc:") {
		t.Errorf("empty Cc should be omitted:\n%s", raw)
	}
}

func TestBuildMIMEWithCc(t *testing.T) {
	raw := string(BuildMIME(
		MIMEHeaders{To: "a@b.test", Cc: "c@d.test", Subject: "Hi"},
		[]byte("body"),
		nil,
	))
	if !strings.Contains(raw, "Cc: c@d.test\r\n") {
		t.Errorf("non-empty Cc should be emitted:\n%s", raw)
	}
}

func TestBuildMIMEWithAttachments(t *testing.T) {
	content := []byte("PDF-RAW-BYTES")
	raw := string(BuildMIME(
		MIMEHeaders{To: "a@b.test", Subject: "Report"},
		[]byte("see attached"),
		[]MIMEAttachment{{Filename: "r.pdf", ContentType: "application/pdf", Content: content}},
	))
	boundary := boundaryOf(t, raw)

	for _, want := range []string{
		"Content-Type: multipart/mixed; boundary=\"" + boundary + "\"\r\n",
		"--" + boundary + "\r\n",
		"Content-Type: text/plain; charset=\"UTF-8\"\r\n",
		"see attached",
		"Content-Type: application/pdf; name=\"r.pdf\"\r\n",
		"Content-Transfer-Encoding: base64\r\n",
		"Content-Disposition: attachment; filename=\"r.pdf\"\r\n",
		"--" + boundary + "--\r\n",
	} {
		if !strings.Contains(raw, want) {
			t.Errorf("multipart MIME missing %q:\n%s", want, raw)
		}
	}
	// The attachment body must be the standard base64 of the content.
	if !strings.Contains(raw, base64.StdEncoding.EncodeToString(content)) {
		t.Errorf("attachment body should be base64 of content:\n%s", raw)
	}
	// Boundary delimiters: opening for body, opening for attachment, closing.
	if got := strings.Count(raw, "--"+boundary); got < 3 {
		t.Errorf("expected at least 3 boundary delimiters, got %d:\n%s", got, raw)
	}
}

func TestBuildMIMEDefaultsAttachmentContentType(t *testing.T) {
	raw := string(BuildMIME(
		MIMEHeaders{To: "a@b.test", Subject: "x"},
		[]byte("b"),
		[]MIMEAttachment{{Filename: "blob", Content: []byte("z")}},
	))
	if !strings.Contains(raw, "Content-Type: application/octet-stream; name=\"blob\"\r\n") {
		t.Errorf("missing content type default should be octet-stream:\n%s", raw)
	}
}

// A newline in a header value must be folded to a space so it cannot inject an
// extra header (header-injection guard).
func TestBuildMIMESanitizesHeaderInjection(t *testing.T) {
	raw := string(BuildMIME(
		MIMEHeaders{To: "a@b.test\r\nBcc: evil@x.test", Subject: "ok"},
		[]byte("b"),
		nil,
	))
	// The injected newline must not start a real header line; folding it to
	// whitespace keeps "Bcc:" inside the To value, not as its own header.
	if strings.Contains(raw, "\r\nBcc:") {
		t.Errorf("header injection via newline must be neutralized:\n%s", raw)
	}
	if !strings.Contains(raw, "To: a@b.test  Bcc: evil@x.test\r\n") {
		t.Errorf("injected CRLF should fold into the To value:\n%s", raw)
	}
}

func TestWrapBase64Width(t *testing.T) {
	long := strings.Repeat("A", 200)
	got := wrapBase64(long)
	for _, line := range strings.Split(got, "\r\n") {
		if len(line) > 76 {
			t.Errorf("base64 line exceeds 76 chars: %d", len(line))
		}
	}
}

func TestAppendAttachmentWrapsExistingBody(t *testing.T) {
	orig := []byte("To: x@y.test\r\nSubject: Hi\r\n\r\noriginal body")
	out := string(AppendAttachment(orig, MIMEAttachment{
		Filename:    "extra.txt",
		ContentType: "text/plain",
		Content:     []byte("more data"),
	}))
	boundary := boundaryOf(t, out)

	for _, want := range []string{
		"To: x@y.test\r\n",
		"Subject: Hi\r\n",
		"Content-Type: multipart/mixed; boundary=\"" + boundary + "\"\r\n",
		"original body",
		"Content-Disposition: attachment; filename=\"extra.txt\"\r\n",
		"--" + boundary + "--\r\n",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("appended MIME missing %q:\n%s", want, out)
		}
	}
	if !strings.Contains(out, base64.StdEncoding.EncodeToString([]byte("more data"))) {
		t.Errorf("new attachment body should be base64-encoded:\n%s", out)
	}
}

// Appending to a message that is already multipart/mixed must still nest the
// original content as the first part and add the new attachment (the rebuild is
// a single transform, never an in-place boundary edit).
func TestAppendAttachmentPreservesOriginalContentType(t *testing.T) {
	orig := []byte("Subject: Hi\r\nContent-Type: text/html; charset=\"UTF-8\"\r\n\r\n<b>hi</b>")
	out := string(AppendAttachment(orig, MIMEAttachment{Filename: "a.bin", Content: []byte("x")}))
	if !strings.Contains(out, "Content-Type: text/html; charset=\"UTF-8\"\r\n") {
		t.Errorf("original content type should be preserved as the first part:\n%s", out)
	}
	if !strings.Contains(out, "<b>hi</b>") {
		t.Errorf("original body should be preserved:\n%s", out)
	}
	// The replaced top-level content type is now multipart/mixed.
	if !strings.Contains(out, "Content-Type: multipart/mixed;") {
		t.Errorf("top-level type should become multipart/mixed:\n%s", out)
	}
}
