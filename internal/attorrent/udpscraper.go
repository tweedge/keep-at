package attorrent

import (
	"context"
	"fmt"
	"sync"

	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/tracker"
	"github.com/anacrolix/torrent/types/infohash"
)

// UDPScraper scrapes UDP trackers (BEP 15) while reusing one
// tracker.Client - and the UDP socket it owns - per distinct tracker,
// rather than creating and closing one for every single scrape call.
//
// The wire protocol is explicitly designed for this: each request carries
// a random transaction ID, and responses are dispatched back to the
// matching in-flight caller by that ID (protected by its own mutex - see
// tracker/udp's Dispatcher), so concurrent scrapes against the same
// tracker from multiple goroutines are safe on one shared client.
//
// Reusing clients isn't just an efficiency win: creating a fresh UDP
// socket per scrape, thousands of times over a long catalog scan, was
// observed in real testing to eventually exhaust available ephemeral
// ports/file descriptors and start failing with "address already in use"
// - a slow resource leak that only showed up hours into a scan, not a
// handful of candidates in.
type UDPScraper struct {
	mu      sync.Mutex
	clients map[string]tracker.Client
}

// NewUDPScraper returns an empty scraper. Clients are created lazily, one
// per distinct tracker URL, the first time that tracker is scraped.
func NewUDPScraper() *UDPScraper {
	return &UDPScraper{clients: make(map[string]tracker.Client)}
}

// Scrape queries a single UDP tracker for every hash in one request,
// reusing (or creating, on first use) a client cached by announceURL.
func (s *UDPScraper) Scrape(ctx context.Context, announceURL string, hashes []metainfo.Hash) (map[metainfo.Hash]SwarmCounts, error) {
	client, err := s.client(announceURL)
	if err != nil {
		return nil, fmt.Errorf("attorrent: creating udp tracker client for %s: %w", announceURL, err)
	}

	ihs := make([]infohash.T, len(hashes))
	for i, h := range hashes {
		ihs[i] = infohash.T(h)
	}

	resp, err := client.Scrape(ctx, ihs)
	if err != nil {
		return nil, fmt.Errorf("attorrent: udp scrape %s: %w", announceURL, err)
	}

	out := make(map[metainfo.Hash]SwarmCounts, len(hashes))
	for i, h := range hashes {
		if i >= len(resp) {
			break
		}
		out[h] = SwarmCounts{
			Seeders:   int(resp[i].Seeders),
			Leechers:  int(resp[i].Leechers),
			Completed: int(resp[i].Completed),
		}
	}
	return out, nil
}

func (s *UDPScraper) client(announceURL string) (tracker.Client, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if c, ok := s.clients[announceURL]; ok {
		return c, nil
	}

	c, err := tracker.NewClient(announceURL, tracker.NewClientOpts{})
	if err != nil {
		return nil, err
	}
	s.clients[announceURL] = c
	return c, nil
}

// Close releases every cached client's underlying socket. Safe to call
// once, typically when the owning Engine shuts down.
func (s *UDPScraper) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()

	var errs []error
	for url, c := range s.clients {
		if err := c.Close(); err != nil {
			errs = append(errs, fmt.Errorf("%s: %w", url, err))
		}
	}
	s.clients = make(map[string]tracker.Client)

	if len(errs) > 0 {
		return fmt.Errorf("attorrent: closing udp scraper clients: %v", errs)
	}
	return nil
}
