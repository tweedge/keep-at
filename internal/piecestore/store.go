// Package piecestore is keep-at's storage backend for anacrolix/torrent. Every
// piece is gzip-compressed once it's verified and stored under a per-torrent
// directory named after the infohash; there's no attempt to reconstruct the
// original file layout, since the plan explicitly says stored data doesn't
// need to be locally readable. That constraint is what makes per-piece
// compression simple: keep-at owns the byte layout end to end.
package piecestore

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	g "github.com/anacrolix/generics"
	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/storage"
)

// DefaultDecompressCacheBytes bounds how much decompressed piece data
// keep-at keeps warm in memory to serve chunk requests without re-inflating
// gzip streams repeatedly. Kept modest by default so keep-at stays light on
// small boxes like a Raspberry Pi.
const DefaultDecompressCacheBytes = 64 << 20 // 64 MiB

// Client is a storage.ClientImplCloser backed by per-piece gzip files.
type Client struct {
	baseDir string
	cache   *decompressCache
}

// New creates a piece store rooted at baseDir. baseDir is one of the user's
// configured storage locations, not keep-at's data_dir.
func New(baseDir string) (*Client, error) {
	if err := os.MkdirAll(baseDir, 0o755); err != nil {
		return nil, fmt.Errorf("piecestore: creating base dir: %w", err)
	}
	return &Client{
		baseDir: baseDir,
		cache:   newDecompressCache(DefaultDecompressCacheBytes),
	}, nil
}

func (c *Client) torrentDir(infoHash metainfo.Hash) string {
	return filepath.Join(c.baseDir, infoHash.HexString())
}

func (c *Client) OpenTorrent(_ context.Context, info *metainfo.Info, infoHash metainfo.Hash) (storage.TorrentImpl, error) {
	dir := c.torrentDir(infoHash)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return storage.TorrentImpl{}, fmt.Errorf("piecestore: creating torrent dir: %w", err)
	}

	hex := infoHash.HexString()

	return storage.TorrentImpl{
		PieceWithHash: func(p metainfo.Piece, _ g.Option[[]byte]) storage.PieceImpl {
			return newGzipPiece(dir, p.Index(), p.Length(), c.cache, hex)
		},
		Close: func() error { return nil },
	}, nil
}

func (c *Client) Close() error {
	return nil
}

// DeleteTorrent removes all stored data for a torrent, compressed and
// staging alike. Called when a torrent is swapped out or removed from the
// Academic Torrents catalog.
func (c *Client) DeleteTorrent(infoHash metainfo.Hash) error {
	dir := c.torrentDir(infoHash)
	if err := os.RemoveAll(dir); err != nil {
		return fmt.Errorf("piecestore: removing torrent dir %s: %w", dir, err)
	}
	return nil
}

// DiskUsage reports the actual on-disk (compressed) bytes used by a
// torrent's stored pieces, for space-accounting purposes. Staging files
// (incomplete pieces) are included since they occupy real disk space too.
func (c *Client) DiskUsage(infoHash metainfo.Hash) (int64, error) {
	dir := c.torrentDir(infoHash)
	var total int64
	err := filepath.Walk(dir, func(_ string, info os.FileInfo, err error) error {
		if err != nil {
			if os.IsNotExist(err) {
				return nil
			}
			return err
		}
		if !info.IsDir() {
			total += info.Size()
		}
		return nil
	})
	if err != nil {
		return 0, fmt.Errorf("piecestore: computing disk usage for %s: %w", dir, err)
	}
	return total, nil
}

// DiskUsageAll reports the total actual on-disk (compressed) bytes in the
// whole location, summing every torrent's stored pieces (staging included).
// This is what keep-at's space accounting subtracts from a location's limit:
// since pieces are gzip-compressed, the real footprint is the on-disk bytes,
// not the nominal torrent sizes.
func (c *Client) DiskUsageAll() (int64, error) {
	var total int64
	err := filepath.Walk(c.baseDir, func(_ string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if !info.IsDir() {
			total += info.Size()
		}
		return nil
	})
	if err != nil {
		return 0, fmt.Errorf("piecestore: computing disk usage for %s: %w", c.baseDir, err)
	}
	return total, nil
}

// CompletedPieceCount counts how many of a torrent's pieces are fully stored
// (compressed, final .piece.gz files). Incomplete pieces live in a staging/
// subdirectory and don't count. Used to detect stalled downloads: a torrent
// that isn't gaining pieces is stuck.
func (c *Client) CompletedPieceCount(infoHash metainfo.Hash) (int, error) {
	dir := c.torrentDir(infoHash)
	entries, err := os.ReadDir(dir)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, nil
		}
		return 0, fmt.Errorf("piecestore: listing torrent dir %s: %w", dir, err)
	}
	count := 0
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		if len(e.Name()) > len(".piece.gz") && e.Name()[len(e.Name())-len(".piece.gz"):] == ".piece.gz" {
			count++
		}
	}
	return count, nil
}
