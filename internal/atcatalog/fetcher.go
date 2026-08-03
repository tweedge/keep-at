package atcatalog

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
	"os"
	"time"
)

// Fetcher wraps database.xml retrieval with an on-disk cache, so keep-at only
// hits Academic Torrents when the cache is missing or older than MaxAge.
type Fetcher struct {
	CachePath  string
	HTTPClient *http.Client
	UserAgent  string
	// URL overrides DefaultURL; tests point this at an httptest server.
	URL string
}

// Load returns a Catalog, refreshing from Academic Torrents first if the
// cache is missing or older than maxAge.
func (f *Fetcher) Load(ctx context.Context, maxAge time.Duration) (Catalog, error) {
	if fresh, ok := f.cacheIsFresh(maxAge); ok && fresh {
		if cat, err := f.loadFromCache(); err == nil {
			return cat, nil
		}
		// Fall through to a real fetch if the cache is unreadable/corrupt.
	}

	url := f.URL
	if url == "" {
		url = DefaultURL
	}

	data, err := FetchRaw(ctx, f.HTTPClient, url, f.UserAgent)
	if err != nil {
		if cat, cacheErr := f.loadFromCache(); cacheErr == nil {
			return cat, fmt.Errorf("atcatalog: refresh failed (%w), served stale cache from %s", err, f.CachePath)
		}
		return Catalog{}, err
	}

	if err := SaveRaw(f.CachePath, data); err != nil {
		return Catalog{}, err
	}

	return Parse(bytes.NewReader(data))
}

func (f *Fetcher) cacheIsFresh(maxAge time.Duration) (fresh bool, ok bool) {
	info, err := os.Stat(f.CachePath)
	if err != nil {
		return false, false
	}
	return time.Since(info.ModTime()) < maxAge, true
}

func (f *Fetcher) loadFromCache() (Catalog, error) {
	data, err := os.ReadFile(f.CachePath)
	if err != nil {
		return Catalog{}, fmt.Errorf("atcatalog: reading cache %s: %w", f.CachePath, err)
	}
	return Parse(bytes.NewReader(data))
}
