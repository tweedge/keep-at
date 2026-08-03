package piecestore

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"

	g "github.com/anacrolix/generics"
	"github.com/anacrolix/torrent/metainfo"
)

func TestPieceLifecycle(t *testing.T) {
	dir := t.TempDir()
	client, err := New(dir)
	if err != nil {
		t.Fatalf("New: %v", err)
	}

	info := &metainfo.Info{PieceLength: 16, Length: 16, Name: "test", Pieces: make([]byte, 20)}
	infoHash := metainfo.HashBytes([]byte("fake-infohash-for-test"))

	torrentImpl, err := client.OpenTorrent(context.Background(), info, infoHash)
	if err != nil {
		t.Fatalf("OpenTorrent: %v", err)
	}
	defer torrentImpl.Close()

	piece := torrentImpl.PieceWithHash(info.Piece(0), g.None[[]byte]())

	want := []byte("0123456789abcdef")
	if _, err := piece.WriteAt(want[:8], 0); err != nil {
		t.Fatalf("WriteAt first half: %v", err)
	}
	if _, err := piece.WriteAt(want[8:], 8); err != nil {
		t.Fatalf("WriteAt second half: %v", err)
	}

	got := make([]byte, len(want))
	if _, err := piece.ReadAt(got, 0); err != nil {
		t.Fatalf("ReadAt while staging: %v", err)
	}
	if !bytes.Equal(got, want) {
		t.Fatalf("staging read mismatch: got %q want %q", got, want)
	}

	if piece.Completion().Complete {
		t.Fatalf("piece should not be complete before MarkComplete")
	}

	if err := piece.MarkComplete(); err != nil {
		t.Fatalf("MarkComplete: %v", err)
	}
	if !piece.Completion().Complete {
		t.Fatalf("piece should be complete after MarkComplete")
	}

	compressedPath := filepath.Join(dir, infoHash.HexString(), "0.piece.gz")
	if _, err := os.Stat(compressedPath); err != nil {
		t.Fatalf("expected compressed piece at %s: %v", compressedPath, err)
	}
	stagingPath := filepath.Join(dir, infoHash.HexString(), "staging", "0.piece")
	if _, err := os.Stat(stagingPath); !os.IsNotExist(err) {
		t.Fatalf("expected staging file to be removed, stat err: %v", err)
	}

	gotAfterCompress := make([]byte, len(want))
	if _, err := piece.ReadAt(gotAfterCompress, 0); err != nil {
		t.Fatalf("ReadAt after compress: %v", err)
	}
	if !bytes.Equal(gotAfterCompress, want) {
		t.Fatalf("compressed read mismatch: got %q want %q", gotAfterCompress, want)
	}

	partial := make([]byte, 4)
	if _, err := piece.ReadAt(partial, 6); err != nil {
		t.Fatalf("partial ReadAt after compress: %v", err)
	}
	if !bytes.Equal(partial, want[6:10]) {
		t.Fatalf("partial read mismatch: got %q want %q", partial, want[6:10])
	}

	usage, err := client.DiskUsage(infoHash)
	if err != nil {
		t.Fatalf("DiskUsage: %v", err)
	}
	if usage <= 0 {
		t.Fatalf("expected positive disk usage, got %d", usage)
	}

	if err := client.DeleteTorrent(infoHash); err != nil {
		t.Fatalf("DeleteTorrent: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dir, infoHash.HexString())); !os.IsNotExist(err) {
		t.Fatalf("expected torrent dir removed, stat err: %v", err)
	}
}

func TestMarkNotCompleteAllowsRewrite(t *testing.T) {
	dir := t.TempDir()
	client, err := New(dir)
	if err != nil {
		t.Fatalf("New: %v", err)
	}

	info := &metainfo.Info{PieceLength: 4, Length: 4, Name: "test", Pieces: make([]byte, 20)}
	infoHash := metainfo.HashBytes([]byte("another-fake-infohash"))
	torrentImpl, err := client.OpenTorrent(context.Background(), info, infoHash)
	if err != nil {
		t.Fatalf("OpenTorrent: %v", err)
	}

	piece := torrentImpl.PieceWithHash(info.Piece(0), g.None[[]byte]())

	if _, err := piece.WriteAt([]byte("bad!"), 0); err != nil {
		t.Fatalf("WriteAt: %v", err)
	}
	if err := piece.MarkComplete(); err != nil {
		t.Fatalf("MarkComplete: %v", err)
	}
	if err := piece.MarkNotComplete(); err != nil {
		t.Fatalf("MarkNotComplete: %v", err)
	}
	if piece.Completion().Complete {
		t.Fatalf("piece should not be complete after MarkNotComplete")
	}

	if _, err := piece.WriteAt([]byte("good"), 0); err != nil {
		t.Fatalf("rewrite WriteAt: %v", err)
	}
	got := make([]byte, 4)
	if _, err := piece.ReadAt(got, 0); err != nil {
		t.Fatalf("rewrite ReadAt: %v", err)
	}
	if !bytes.Equal(got, []byte("good")) {
		t.Fatalf("rewrite mismatch: got %q", got)
	}
}
