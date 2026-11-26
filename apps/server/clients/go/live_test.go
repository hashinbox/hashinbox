package hashinbox

import (
	"context"
	"os"
	"testing"
	"time"
)

// TestLive smokes a running server: HASHINBOX_URL, and with
// HASHINBOX_KEY_ID and HASHINBOX_SECRET a key's own session too.
func TestLive(t *testing.T) {
	base := os.Getenv("HASHINBOX_URL")
	if base == "" {
		t.Skip("HASHINBOX_URL not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	version, err := VersionGet(ctx, NewClient(base), VersionGetRequest{})
	if err != nil || version.Version.Version == "" {
		t.Fatal(version, err)
	}
	id, value := os.Getenv("HASHINBOX_KEY_ID"), os.Getenv("HASHINBOX_SECRET")
	if id == "" || value == "" {
		return
	}
	secret, err := ParseSecret(id, value)
	if err != nil {
		t.Fatal(err)
	}
	client := NewClient(base, WithSecret(secret))
	read, err := SessionGet(ctx, client, SessionGetRequest{})
	if err != nil || read.Key == nil || read.Key.ID != id {
		t.Fatal(read, err)
	}
}
