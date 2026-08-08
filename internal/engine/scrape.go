package engine

import (
	"context"
	"fmt"
	"net"
	"net/url"
	"strings"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/buildinfo"
)

const scrapeTimeout = 15 * time.Second

// scrapeSwarm queries trackers in order and returns counts from the first
// one that answers successfully. Academic Torrents always lists its own
// tracker first in the .torrent files it serves, so in the overwhelming
// common case this means exactly one lightweight scrape request per
// torrent, not a fan-out to every tracker in the file.
func (e *Engine) scrapeSwarm(ctx context.Context, trackers []string, infoHash metainfo.Hash) (attorrent.SwarmCounts, error) {
	// Reuse a cached count from a previous scan when it's still fresh. This
	// is what keeps repeat scans cheap: the vast majority of the catalog
	// hasn't changed since last week, so we skip the rate-limited request to
	// Academic Torrents' tracker entirely.
	if cached, ok := e.swarmCache.get(infoHash); ok {
		return cached, nil
	}

	var lastErr error
	for _, tracker := range trackers {
		if isAcademicTorrentsHost(tracker) && e.torrentFetcher.Limiter != nil {
			if err := e.torrentFetcher.Limiter.Wait(ctx); err != nil {
				return attorrent.SwarmCounts{}, err
			}
		}

		callCtx, cancel := context.WithTimeout(ctx, scrapeTimeout)
		counts, err := attorrent.Scrape(callCtx, e.httpClient, e.udpScraper, buildinfo.ScraperUserAgent(), tracker, []metainfo.Hash{infoHash})
		cancel()
		if err != nil {
			lastErr = err
			continue
		}
		if c, ok := counts[infoHash]; ok {
			e.swarmCache.put(infoHash, c)
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

// rateLimitedTrackerDialer wraps a plain net.Dialer with e's Academic
// Torrents rate limiter, gated on the destination address rather than a
// URL. anacrolix/torrent's own client re-announces to every tracker in a
// torrent's spec on its own schedule, entirely outside scrapeSwarm and
// attorrent.Fetcher - without this, that automatic announcing hits
// academictorrents.com's tracker uncontrolled, which in real testing
// against the full catalog got keep-at rate-limited (HTTP 429) by AT
// itself. Passed to ClientConfig.TrackerDialContext on every torrent
// client keep-at creates (see newTorrentClient) so every path to AT's
// tracker - our own scrapes and the library's automatic announces alike -
// shares one budget.
func (e *Engine) rateLimitedTrackerDialer(ctx context.Context, network, addr string) (net.Conn, error) {
	if isAcademicTorrentsAddr(addr) && e.torrentFetcher.Limiter != nil {
		if err := e.torrentFetcher.Limiter.Wait(ctx); err != nil {
			return nil, err
		}
	}
	return (&net.Dialer{}).DialContext(ctx, network, addr)
}

func isAcademicTorrentsAddr(addr string) bool {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		host = addr
	}
	return strings.Contains(host, "academictorrents.com")
}
