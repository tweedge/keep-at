package engine

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
)

func TestResolveUserAnnounce(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/apiv2/userannounce" {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		// The API key must arrive as a Cookie header with uid and pass.
		gotCookie := r.Header.Get("Cookie")
		if !strings.Contains(gotCookie, "uid=19791") || !strings.Contains(gotCookie, "pass=596a") {
			http.Error(w, "missing key cookies", http.StatusUnauthorized)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"userannounce":"https://academictorrents.com/announce.php?passkey=abc123"}`)
	}))
	defer srv.Close()

	client := srv.Client()

	orig := userAnnounceEndpoint
	userAnnounceEndpoint = srv.URL + "/apiv2/userannounce"
	defer func() { userAnnounceEndpoint = orig }()

	ua, ua6, err := resolveUserAnnounce(context.Background(), client, "uid=19791;pass=596a3c9973a5eecf2dcd68d1d7e55493")
	if err != nil {
		t.Fatalf("resolveUserAnnounce: %v", err)
	}
	if ua != "https://academictorrents.com/announce.php?passkey=abc123" {
		t.Errorf("user announce = %q", ua)
	}
	if ua6 != "https://ipv6.academictorrents.com/announce.php?passkey=abc123" {
		t.Errorf("ipv6 user announce = %q", ua6)
	}
}

func TestResolveUserAnnounceErrors(t *testing.T) {
	orig := userAnnounceEndpoint
	userAnnounceEndpoint = "https://example.invalid/apiv2/userannounce"
	defer func() { userAnnounceEndpoint = orig }()

	if _, _, err := resolveUserAnnounce(context.Background(), &http.Client{}, "uid=1;pass=x"); err == nil {
		t.Fatal("expected an error for an unreachable endpoint")
	}
}

func TestAtAnnounceURLHostRestriction(t *testing.T) {
	ua := "https://academictorrents.com/announce.php?passkey=abc123"
	ua6 := "https://ipv6.academictorrents.com/announce.php?passkey=abc123"

	cases := []struct {
		announce string
		want     string
	}{
		{"https://academictorrents.com/announce.php", ua},
		{"https://ipv6.academictorrents.com/announce.php", ua6},
		{"http://academictorrents.com/announce.php", ""}, // not https
		{"https://evilacademictorrents.com/announce.php", ""},
		{"https://academictorrents.com.evil.example/announce.php", ""},
		{"https://academictorrents.com.evil.example/announce.php", ""},
		{"https://udp.example.com/announce", ""},       // third-party
		{"udp://academictorrents.com:1337", ""},        // not https
		{"https://tracker.opentrackr.org:1337/announce", ""},
		{"not a url", ""},
	}
	for _, c := range cases {
		if got := atAnnounceURL(c.announce, ua, ua6); got != c.want {
			t.Errorf("atAnnounceURL(%q) = %q, want %q", c.announce, got, c.want)
		}
	}
}

func TestKeyedTrackersOnlyRewritesATHosts(t *testing.T) {
	ua := "https://academictorrents.com/announce.php?passkey=abc123"
	ua6 := "https://ipv6.academictorrents.com/announce.php?passkey=abc123"

	trackers := [][]string{
		{"https://academictorrents.com/announce.php"},
		{"https://ipv6.academictorrents.com/announce.php"},
		{"udp://tracker.opentrackr.org:1337/announce"},
		{"https://academictorrents.com/announce.php", "udp://tracker.publicbt.com:80/announce"},
	}

	got := keyedTrackers(trackers, ua, ua6)
	want := [][]string{
		{ua},
		{ua6},
		{"udp://tracker.opentrackr.org:1337/announce"},
		{ua, "udp://tracker.publicbt.com:80/announce"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("keyedTrackers:\n got %v\nwant %v", got, want)
	}
}

func TestKeyedTrackersNoKeyPassesThrough(t *testing.T) {
	trackers := [][]string{
		{"https://academictorrents.com/announce.php"},
		{"udp://tracker.opentrackr.org:1337/announce"},
	}
	if got := keyedTrackers(trackers, "", ""); !reflect.DeepEqual(got, trackers) {
		t.Errorf("keyedTrackers with no key changed the list: %v", got)
	}
}

func TestSanitizeAPIKey(t *testing.T) {
	if s := sanitizeAPIKey("uid=19791;pass=596a3c9973a5eecf2dcd68d1d7e55493"); strings.Contains(s, "596a3c9973a5eecf2dcd68d1d7e55493") {
		t.Errorf("sanitizeAPIKey leaked the pass: %q", s)
	}
	if !strings.Contains(sanitizeAPIKey("uid=19791;pass=x"), "pass=<redacted>") {
		t.Errorf("sanitizeAPIKey didn't redact pass: %q", sanitizeAPIKey("uid=19791;pass=x"))
	}
}
