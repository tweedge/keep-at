package atcatalog

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestFetcherUsesFreshCacheWithoutNetwork(t *testing.T) {
	dir := t.TempDir()
	cachePath := filepath.Join(dir, "database.xml")
	if err := os.WriteFile(cachePath, []byte(sampleXML), 0o644); err != nil {
		t.Fatalf("seeding cache: %v", err)
	}

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Fatalf("network should not be hit when cache is fresh")
	}))
	defer server.Close()

	fetcher := &Fetcher{CachePath: cachePath, HTTPClient: server.Client(), URL: server.URL}
	cat, err := fetcher.Load(context.Background(), time.Hour)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(cat.Items) != 1 {
		t.Fatalf("expected cached item, got %d items", len(cat.Items))
	}
}

func TestFetcherRefreshesStaleCache(t *testing.T) {
	dir := t.TempDir()
	cachePath := filepath.Join(dir, "database.xml")
	if err := os.WriteFile(cachePath, []byte("<rss><channel></channel></rss>"), 0o644); err != nil {
		t.Fatalf("seeding cache: %v", err)
	}
	staleTime := time.Now().Add(-24 * time.Hour)
	if err := os.Chtimes(cachePath, staleTime, staleTime); err != nil {
		t.Fatalf("chtimes: %v", err)
	}

	hits := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		_, _ = w.Write([]byte(sampleXML))
	}))
	defer server.Close()

	fetcher := &Fetcher{CachePath: cachePath, HTTPClient: server.Client(), URL: server.URL}
	cat, err := fetcher.Load(context.Background(), time.Minute)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if hits != 1 {
		t.Fatalf("expected exactly 1 network hit, got %d", hits)
	}
	if len(cat.Items) != 1 {
		t.Fatalf("expected 1 item after refresh, got %d", len(cat.Items))
	}
}

func TestFetcherFallsBackToStaleCacheOnNetworkError(t *testing.T) {
	dir := t.TempDir()
	cachePath := filepath.Join(dir, "database.xml")
	if err := os.WriteFile(cachePath, []byte(sampleXML), 0o644); err != nil {
		t.Fatalf("seeding cache: %v", err)
	}
	staleTime := time.Now().Add(-24 * time.Hour)
	if err := os.Chtimes(cachePath, staleTime, staleTime); err != nil {
		t.Fatalf("chtimes: %v", err)
	}

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer server.Close()

	fetcher := &Fetcher{CachePath: cachePath, HTTPClient: server.Client(), URL: server.URL}
	cat, err := fetcher.Load(context.Background(), time.Minute)
	if err == nil {
		t.Fatalf("expected an error surfaced alongside the stale-cache fallback")
	}
	if len(cat.Items) != 1 {
		t.Fatalf("expected stale cache to still be usable, got %d items", len(cat.Items))
	}
}
