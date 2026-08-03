package attorrent

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/tracker"
	"github.com/anacrolix/torrent/types/infohash"
)

// SwarmCounts is what keep-at actually cares about from a tracker: is anyone
// seeding, and how many. Selection logic in the selector package treats
// Seeders as the availability signal and Leechers/Completed as tie-breakers.
type SwarmCounts struct {
	Seeders   int
	Leechers  int
	Completed int
}

// ScrapeHTTP queries a single HTTP(S) tracker for every hash in one request,
// using the BEP 48 scrape convention (announce URL with "announce" replaced
// by "scrape" in its final path segment). Batching every hash keep-at cares
// about into one request is what keeps a full catalog scan from turning
// into thousands of individual tracker hits.
func ScrapeHTTP(ctx context.Context, client *http.Client, announceURL string, hashes []metainfo.Hash) (map[metainfo.Hash]SwarmCounts, error) {
	scrapeURL, ok := deriveHTTPScrapeURL(announceURL)
	if !ok {
		return nil, fmt.Errorf("attorrent: tracker %s does not follow the announce/scrape URL convention", announceURL)
	}

	query := url.Values{}
	for _, h := range hashes {
		query.Add("info_hash", string(h.Bytes()))
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, scrapeURL+"?"+query.Encode(), nil)
	if err != nil {
		return nil, fmt.Errorf("attorrent: building scrape request: %w", err)
	}

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("attorrent: scrape request to %s: %w", scrapeURL, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("attorrent: scrape %s returned %s", scrapeURL, resp.Status)
	}

	var decoded struct {
		Files map[string]struct {
			Seeders   int `bencode:"complete"`
			Completed int `bencode:"downloaded"`
			Leechers  int `bencode:"incomplete"`
		} `bencode:"files"`
	}
	if err := bencode.NewDecoder(resp.Body).Decode(&decoded); err != nil {
		return nil, fmt.Errorf("attorrent: decoding scrape response from %s: %w", scrapeURL, err)
	}

	out := make(map[metainfo.Hash]SwarmCounts, len(hashes))
	for _, h := range hashes {
		if entry, ok := decoded.Files[string(h.Bytes())]; ok {
			out[h] = SwarmCounts{Seeders: entry.Seeders, Leechers: entry.Leechers, Completed: entry.Completed}
		}
	}
	return out, nil
}

// ScrapeUDP queries a single UDP tracker for every hash in one request (BEP
// 15), delegating the wire protocol to anacrolix/torrent's tracker client.
func ScrapeUDP(ctx context.Context, announceURL string, hashes []metainfo.Hash) (map[metainfo.Hash]SwarmCounts, error) {
	client, err := tracker.NewClient(announceURL, tracker.NewClientOpts{})
	if err != nil {
		return nil, fmt.Errorf("attorrent: creating udp tracker client for %s: %w", announceURL, err)
	}
	defer client.Close()

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

// Scrape picks the right protocol for announceURL and scrapes it.
func Scrape(ctx context.Context, client *http.Client, announceURL string, hashes []metainfo.Hash) (map[metainfo.Hash]SwarmCounts, error) {
	switch {
	case strings.HasPrefix(announceURL, "http://"), strings.HasPrefix(announceURL, "https://"):
		return ScrapeHTTP(ctx, client, announceURL, hashes)
	case strings.HasPrefix(announceURL, "udp://"):
		return ScrapeUDP(ctx, announceURL, hashes)
	default:
		return nil, fmt.Errorf("attorrent: unsupported tracker scheme in %s", announceURL)
	}
}

// deriveHTTPScrapeURL implements the BEP 48 convention: in the final path
// segment of the announce URL, replace "announce" with "scrape". Trackers
// that don't have "announce" in their path (a minority) don't support this
// convention and are skipped by callers.
func deriveHTTPScrapeURL(announceURL string) (string, bool) {
	lastSlash := strings.LastIndex(announceURL, "/")
	if lastSlash == -1 {
		return "", false
	}
	segment := announceURL[lastSlash+1:]
	if !strings.Contains(segment, "announce") {
		return "", false
	}
	return announceURL[:lastSlash+1] + strings.Replace(segment, "announce", "scrape", 1), true
}
