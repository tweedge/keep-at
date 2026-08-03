package engine

import (
	"context"
	"fmt"
	"net/url"
	"strings"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/mimisbaeti/internal/attorrent"
)

const scrapeTimeout = 15 * time.Second

// scrapeSwarm queries trackers in order and returns counts from the first
// one that answers successfully. Academic Torrents always lists its own
// tracker first in the .torrent files it serves, so in the overwhelming
// common case this means exactly one lightweight scrape request per
// torrent, not a fan-out to every tracker in the file.
func (e *Engine) scrapeSwarm(ctx context.Context, trackers []string, infoHash metainfo.Hash) (attorrent.SwarmCounts, error) {
	var lastErr error
	for _, tracker := range trackers {
		if isAcademicTorrentsHost(tracker) && e.torrentFetcher.Limiter != nil {
			if err := e.torrentFetcher.Limiter.Wait(ctx); err != nil {
				return attorrent.SwarmCounts{}, err
			}
		}

		callCtx, cancel := context.WithTimeout(ctx, scrapeTimeout)
		counts, err := attorrent.Scrape(callCtx, e.httpClient, tracker, []metainfo.Hash{infoHash})
		cancel()
		if err != nil {
			lastErr = err
			continue
		}
		if c, ok := counts[infoHash]; ok {
			return c, nil
		}
	}
	if lastErr == nil {
		lastErr = fmt.Errorf("engine: no tracker returned scrape data for %s", infoHash.HexString())
	}
	return attorrent.SwarmCounts{}, lastErr
}

func isAcademicTorrentsHost(trackerURL string) bool {
	u, err := url.Parse(trackerURL)
	if err != nil {
		return false
	}
	return strings.Contains(u.Host, "academictorrents.com")
}
