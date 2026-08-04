package attorrent

import "testing"

func TestUDPScraperReusesClientPerURL(t *testing.T) {
	s := NewUDPScraper()
	defer s.Close()

	c1, err := s.client("udp://tracker.example.com:1337/announce")
	if err != nil {
		t.Fatalf("client: %v", err)
	}
	c2, err := s.client("udp://tracker.example.com:1337/announce")
	if err != nil {
		t.Fatalf("client (second call): %v", err)
	}
	if c1 != c2 {
		t.Fatalf("expected the same cached client for the same URL, got two different instances")
	}

	c3, err := s.client("udp://tracker.other.example.com:6969/announce")
	if err != nil {
		t.Fatalf("client (different url): %v", err)
	}
	if c1 == c3 {
		t.Fatalf("expected a distinct client for a different tracker URL")
	}

	if len(s.clients) != 2 {
		t.Fatalf("expected 2 cached clients, got %d", len(s.clients))
	}
}

func TestUDPScraperCloseClearsCache(t *testing.T) {
	s := NewUDPScraper()

	if _, err := s.client("udp://tracker.example.com:1337/announce"); err != nil {
		t.Fatalf("client: %v", err)
	}
	if len(s.clients) != 1 {
		t.Fatalf("expected 1 cached client before Close, got %d", len(s.clients))
	}

	if err := s.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if len(s.clients) != 0 {
		t.Fatalf("expected the cache to be empty after Close, got %d entries", len(s.clients))
	}
}
