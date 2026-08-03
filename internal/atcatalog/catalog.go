// Package atcatalog fetches and parses Academic Torrents' database.xml, the
// full catalog of everything hosted there. AT explicitly recommends
// downloading this file and searching it locally rather than hitting their
// site per-query, so that's what mimis does: pull it once, cache it to
// disk, and only refetch when it's stale or missing.
package atcatalog

import (
	"context"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"os"
	"time"

	"github.com/anacrolix/torrent/metainfo"
)

// DefaultURL is where Academic Torrents publishes its catalog.
const DefaultURL = "https://academictorrents.com/database.xml"

// Item is one entry from the catalog. AT's database.xml doesn't carry a
// seeder count or an upload date - see the attorrent package for how mimis
// gets those from the .torrent file and tracker scrapes instead.
type Item struct {
	Title       string
	Category    string
	InfoHash    metainfo.Hash
	GUID        string
	Link        string
	Description string
	SizeBytes   int64
}

// Catalog is a fetched, parsed snapshot of database.xml.
type Catalog struct {
	Items     []Item
	FetchedAt time.Time
}

type rssDocument struct {
	Channel struct {
		Items []rssItem `xml:"item"`
	} `xml:"channel"`
}

type rssItem struct {
	Title       string `xml:"title"`
	Category    string `xml:"category"`
	InfoHash    string `xml:"infohash"`
	GUID        string `xml:"guid"`
	Link        string `xml:"link"`
	Description string `xml:"description"`
	Size        int64  `xml:"size"`
}

// Parse decodes a database.xml document. Malformed individual entries
// (a bad infohash, mainly) are skipped rather than failing the whole parse -
// a few thousand entries mean a few bad rows shouldn't take down a scan.
func Parse(r io.Reader) (Catalog, error) {
	var doc rssDocument
	if err := xml.NewDecoder(r).Decode(&doc); err != nil {
		return Catalog{}, fmt.Errorf("atcatalog: decoding database.xml: %w", err)
	}

	items := make([]Item, 0, len(doc.Channel.Items))
	for _, raw := range doc.Channel.Items {
		var hash metainfo.Hash
		if err := hash.FromHexString(raw.InfoHash); err != nil {
			continue
		}
		items = append(items, Item{
			Title:       raw.Title,
			Category:    raw.Category,
			InfoHash:    hash,
			GUID:        raw.GUID,
			Link:        raw.Link,
			Description: raw.Description,
			SizeBytes:   raw.Size,
		})
	}

	return Catalog{Items: items, FetchedAt: time.Now().UTC()}, nil
}

// FetchRaw downloads database.xml's raw bytes from url. Prefer
// Fetcher.Load for normal operation, which caches this and only calls
// FetchRaw when the cache is stale or missing.
func FetchRaw(ctx context.Context, client *http.Client, url, userAgent string) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("atcatalog: building request: %w", err)
	}
	if userAgent != "" {
		req.Header.Set("User-Agent", userAgent)
	}

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("atcatalog: fetching %s: %w", url, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("atcatalog: fetching %s returned %s", url, resp.Status)
	}

	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("atcatalog: reading database.xml: %w", err)
	}
	return data, nil
}

// SaveRaw persists downloaded database.xml bytes to path, so the next Load
// can skip the network entirely while the cache is still fresh.
func SaveRaw(path string, data []byte) error {
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return fmt.Errorf("atcatalog: writing cache: %w", err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("atcatalog: finalizing cache: %w", err)
	}
	return nil
}
