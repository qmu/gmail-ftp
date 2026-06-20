package gmail

import (
	"encoding/base64"
	"testing"
)

func TestDecodeRawAcceptsBothAlphabets(t *testing.T) {
	payload := []byte("a+b/c==?d") // contains chars that differ between alphabets
	// Standard-alphabet encoding (padded) must still decode via the fallback.
	std := base64.StdEncoding.EncodeToString(payload)
	got, err := decodeRaw(std)
	if err != nil {
		t.Fatalf("decodeRaw(std): %v", err)
	}
	if string(got) != string(payload) {
		t.Errorf("decodeRaw(std) = %q, want %q", got, payload)
	}
	// URL-alphabet encoding (unpadded) decodes directly.
	urlEnc := base64.URLEncoding.WithPadding(base64.NoPadding).EncodeToString(payload)
	got, err = decodeRaw(urlEnc)
	if err != nil {
		t.Fatalf("decodeRaw(url): %v", err)
	}
	if string(got) != string(payload) {
		t.Errorf("decodeRaw(url) = %q, want %q", got, payload)
	}
	// Empty input is nil with no error.
	if b, err := decodeRaw(""); err != nil || b != nil {
		t.Errorf("decodeRaw(\"\") = (%v, %v), want (nil, nil)", b, err)
	}
}
