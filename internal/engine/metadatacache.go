package engine

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/attorrent"
)

// cachedTorrentPath is where keep-at keeps a local copy of a .torrent file
// once fetched, regardless of whether keep-at ends up downloading it. A
// torrent's creation date never changes, so once fetched, keep-at never
// needs to ask Academic Torrents for that same .torrent file again just to
// re-check a candidate's age on a later scan.
func (e *Engine) cachedTorrentPath(infoHash metainfo.Hash) string {
	return filepath.Join(e.cfg.DataDir, "torrent-cache", infoHash.HexString()+".torrent")
}

// fetchMetadata returns a torrent's metadata, preferring keep-at's local cache
// over a fresh request to Academic Torrents.
func (e *Engine) fetchMetadata(ctx context.Context, infoHash metainfo.Hash) (*attorrent.Metadata, error) {
	path := e.cachedTorrentPath(infoHash)
	if data, err := os.ReadFile(path); err == nil {
		if md, parseErr := attorrent.ParseTorrentBytes(data); parseErr == nil {
			return md, nil
		}
		// Corrupt cache entry; fall through and refetch.
	}

	md, err := e.torrentFetcher.FetchTorrent(ctx, infoHash)
	if err != nil {
		return nil, err
	}

	if err := e.saveMetadataCache(infoHash, md); err != nil {
		e.logger.Warn("failed to cache fetched torrent file", "infohash", infoHash.HexString(), "err", err)
	}

	return md, nil
}

func (e *Engine) saveMetadataCache(infoHash metainfo.Hash, md *attorrent.Metadata) error {
	path := e.cachedTorrentPath(infoHash)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("engine: creating torrent cache dir: %w", err)
	}
	var buf bytes.Buffer
	if err := md.MetaInfo.Write(&buf); err != nil {
		return fmt.Errorf("engine: serializing torrent file: %w", err)
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, buf.Bytes(), 0o644); err != nil {
		return fmt.Errorf("engine: writing %s: %w", tmp, err)
	}
	return os.Rename(tmp, path)
}
