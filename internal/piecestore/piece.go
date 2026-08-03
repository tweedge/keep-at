package piecestore

import (
	"bytes"
	"compress/gzip"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"

	"github.com/anacrolix/torrent/storage"
)

// gzipPiece is a single torrent piece backed by two possible on-disk forms:
//
//   - While incomplete, writes land in a sparse "staging" file at their
//     exact byte offsets, since chunks arrive out of order during download.
//   - Once the torrent client verifies the piece hash and calls
//     MarkComplete, the staging file is gzipped into its final form and the
//     staging file is removed. From then on, reads decompress on demand.
//
// This trades some CPU (gzip on write, gunzip on read) for real disk savings
// on the kind of scientific data Academic Torrents hosts, which compresses
// well more often than not.
type gzipPiece struct {
	finalPath   string
	stagingPath string
	length      int64
	cache       *decompressCache
	cacheKey    string

	mu sync.Mutex
}

func newGzipPiece(torrentDir string, index int, length int64, cache *decompressCache, infoHashHex string) *gzipPiece {
	return &gzipPiece{
		finalPath:   filepath.Join(torrentDir, fmt.Sprintf("%d.piece.gz", index)),
		stagingPath: filepath.Join(torrentDir, "staging", fmt.Sprintf("%d.piece", index)),
		length:      length,
		cache:       cache,
		cacheKey:    infoHashHex + "/" + fmt.Sprint(index),
	}
}

func (p *gzipPiece) WriteAt(b []byte, off int64) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()

	if err := os.MkdirAll(filepath.Dir(p.stagingPath), 0o755); err != nil {
		return 0, fmt.Errorf("piecestore: preparing staging dir: %w", err)
	}
	f, err := os.OpenFile(p.stagingPath, os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return 0, fmt.Errorf("piecestore: opening staging file: %w", err)
	}
	defer f.Close()

	n, err := f.WriteAt(b, off)
	if err != nil {
		return n, fmt.Errorf("piecestore: writing staging file: %w", err)
	}
	return n, nil
}

func (p *gzipPiece) ReadAt(b []byte, off int64) (int, error) {
	p.mu.Lock()
	complete := fileExists(p.finalPath)
	p.mu.Unlock()

	if complete {
		return p.readCompressed(b, off)
	}
	return p.readStaging(b, off)
}

func (p *gzipPiece) readStaging(b []byte, off int64) (int, error) {
	f, err := os.Open(p.stagingPath)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, io.EOF
		}
		return 0, fmt.Errorf("piecestore: opening staging file: %w", err)
	}
	defer f.Close()
	return f.ReadAt(b, off)
}

func (p *gzipPiece) readCompressed(b []byte, off int64) (int, error) {
	if cached, ok := p.cache.get(p.cacheKey); ok {
		return copyRange(cached, b, off)
	}

	f, err := os.Open(p.finalPath)
	if err != nil {
		return 0, fmt.Errorf("piecestore: opening compressed piece: %w", err)
	}
	defer f.Close()

	gz, err := gzip.NewReader(f)
	if err != nil {
		return 0, fmt.Errorf("piecestore: creating gzip reader: %w", err)
	}
	defer gz.Close()

	data, err := io.ReadAll(gz)
	if err != nil {
		return 0, fmt.Errorf("piecestore: decompressing piece: %w", err)
	}

	p.cache.put(p.cacheKey, data)
	return copyRange(data, b, off)
}

func copyRange(data, b []byte, off int64) (int, error) {
	if off >= int64(len(data)) {
		return 0, io.EOF
	}
	n := copy(b, data[off:])
	if n < len(b) {
		return n, io.EOF
	}
	return n, nil
}

// MarkComplete is called once the torrent client has verified this piece's
// hash against the staging data. We compress it into its final form here
// and drop the staging file; ReadAt after this point serves from the
// compressed copy.
func (p *gzipPiece) MarkComplete() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	if fileExists(p.finalPath) {
		return nil
	}

	raw, err := os.ReadFile(p.stagingPath)
	if err != nil {
		return fmt.Errorf("piecestore: reading staging file to compress: %w", err)
	}

	var buf bytes.Buffer
	gz, _ := gzip.NewWriterLevel(&buf, gzip.BestSpeed)
	if _, err := gz.Write(raw); err != nil {
		return fmt.Errorf("piecestore: compressing piece: %w", err)
	}
	if err := gz.Close(); err != nil {
		return fmt.Errorf("piecestore: finalizing compressed piece: %w", err)
	}

	if err := os.MkdirAll(filepath.Dir(p.finalPath), 0o755); err != nil {
		return fmt.Errorf("piecestore: preparing final dir: %w", err)
	}
	tmpPath := p.finalPath + ".tmp"
	if err := os.WriteFile(tmpPath, buf.Bytes(), 0o644); err != nil {
		return fmt.Errorf("piecestore: writing compressed piece: %w", err)
	}
	if err := os.Rename(tmpPath, p.finalPath); err != nil {
		return fmt.Errorf("piecestore: finalizing compressed piece rename: %w", err)
	}

	_ = os.Remove(p.stagingPath)
	return nil
}

// MarkNotComplete is called when a piece fails its hash check, so the
// client can re-download it. We drop the staging file so the next write
// starts clean.
func (p *gzipPiece) MarkNotComplete() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.cache.invalidate(p.cacheKey)
	_ = os.Remove(p.stagingPath)
	_ = os.Remove(p.finalPath)
	return nil
}

func (p *gzipPiece) Completion() storage.Completion {
	p.mu.Lock()
	defer p.mu.Unlock()
	return storage.Completion{Ok: true, Complete: fileExists(p.finalPath)}
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}
