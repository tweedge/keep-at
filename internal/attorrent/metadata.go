// Package attorrent handles everything keep-at needs per-torrent from
// Academic Torrents: fetching the .torrent file itself (which carries the
// piece hashes keep-at needs to actually download, plus a creation date used
// as an age proxy) and scraping trackers for seeder/leecher counts without
// joining the swarm.
package attorrent

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"time"

	"github.com/anacrolix/torrent/metainfo"
)

// Metadata is everything keep-at extracts from a .torrent file for selection
// and downloading purposes.
type Metadata struct {
	MetaInfo  *metainfo.MetaInfo
	Info      metainfo.Info
	InfoHash  metainfo.Hash
	Trackers  []string
	CreatedAt time.Time // zero if the torrent has no creation date field
}

// Fetcher downloads .torrent files from Academic Torrents, rate limited so
// a full catalog scan doesn't hammer the site.
type Fetcher struct {
	BaseURL    string
	HTTPClient *http.Client
	UserAgent  string
	Limiter    Limiter
}

// Limiter gates outbound requests to Academic Torrents. It's satisfied by
// *rate.Limiter from golang.org/x/time/rate; defined as an interface here
// so tests don't need a real limiter.
type Limiter interface {
	Wait(ctx context.Context) error
}

// FetchTorrent downloads and parses the .torrent file for infoHash.
func (f *Fetcher) FetchTorrent(ctx context.Context, infoHash metainfo.Hash) (*Metadata, error) {
	if f.Limiter != nil {
		if err := f.Limiter.Wait(ctx); err != nil {
			return nil, fmt.Errorf("attorrent: rate limit wait: %w", err)
		}
	}

	url := f.BaseURL + "/download/" + infoHash.HexString() + ".torrent"
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("attorrent: building request for %s: %w", url, err)
	}
	if f.UserAgent != "" {
		req.Header.Set("User-Agent", f.UserAgent)
	}

	resp, err := f.HTTPClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("attorrent: fetching %s: %w", url, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("attorrent: fetching %s returned %s", url, resp.Status)
	}

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("attorrent: reading %s: %w", url, err)
	}

	return ParseTorrentBytes(body)
}

// ParseTorrentBytes parses a raw .torrent file into Metadata. Exposed
// separately from FetchTorrent so tests and smoke-test tooling can feed in
// a locally cached .torrent file without hitting the network.
func ParseTorrentBytes(body []byte) (*Metadata, error) {
	mi, err := metainfo.Load(bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("attorrent: parsing torrent file: %w", err)
	}

	info, err := mi.UnmarshalInfo()
	if err != nil {
		return nil, fmt.Errorf("attorrent: unmarshalling torrent info: %w", err)
	}

	md := &Metadata{
		MetaInfo: mi,
		Info:     info,
		InfoHash: mi.HashInfoBytes(),
	}

	if mi.CreationDate > 0 {
		md.CreatedAt = time.Unix(mi.CreationDate, 0).UTC()
	}

	for _, tier := range mi.UpvertedAnnounceList() {
		md.Trackers = append(md.Trackers, tier...)
	}

	return md, nil
}
