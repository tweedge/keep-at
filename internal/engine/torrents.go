package engine

import (
	"fmt"
	"net"
	"os"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/buildinfo"
	"github.com/tweedge/keep-at/internal/piecestore"
	"github.com/tweedge/keep-at/internal/state"
)

// resumeHeldTorrents re-adds every torrent keep-at's state says it holds back
// into the BitTorrent client on startup, so it resumes seeding immediately
// rather than waiting for the next scan.
func (e *Engine) resumeHeldTorrents() error {
	for _, held := range e.state.All() {
		store, ok := e.stores[held.StorageLocation]
		if !ok {
			e.logger.Warn("skipping resume: storage location no longer configured",
				"infohash", held.InfoHash.HexString(), "location", held.StorageLocation)
			continue
		}

		mi, err := metainfo.LoadFromFile(e.cachedTorrentPath(held.InfoHash))
		if err != nil {
			e.logger.Warn("skipping resume: could not load cached .torrent file",
				"infohash", held.InfoHash.HexString(), "err", err)
			continue
		}

		if _, err := e.addTorrentSpec(mi, store); err != nil {
			e.logger.Warn("skipping resume: could not add torrent to client",
				"infohash", held.InfoHash.HexString(), "err", err)
			continue
		}
	}
	return nil
}

// addTorrentSpec adds a parsed .torrent to the BitTorrent client, bound to
// a specific storage location, and starts downloading/seeding it. Already
// complete pieces (per the storage backend's own completion tracking) are
// not re-downloaded.
func (e *Engine) addTorrentSpec(mi *metainfo.MetaInfo, store *piecestore.Client) (*torrent.Torrent, error) {
	spec := torrent.TorrentSpecFromMetaInfo(mi)
	spec.Storage = store

	t, _, err := e.torrentClient.AddTorrentSpec(spec)
	if err != nil {
		return nil, fmt.Errorf("engine: adding torrent %s: %w", mi.HashInfoBytes().HexString(), err)
	}
	t.DownloadAll()
	return t, nil
}

// AddCandidate starts downloading and seeding a newly-selected torrent,
// recording it in keep-at's persisted state. Callers are expected to have
// already fetched md via Engine.fetchMetadata, which caches the .torrent
// file this depends on for resuming after a restart.
func (e *Engine) AddCandidate(md *attorrent.Metadata, storageLocation string, sizeBytes int64, title string) error {
	store, ok := e.stores[storageLocation]
	if !ok {
		return fmt.Errorf("engine: unknown storage location %s", storageLocation)
	}

	if err := e.saveMetadataCache(md.InfoHash, md); err != nil {
		return err
	}

	if _, err := e.addTorrentSpec(md.MetaInfo, store); err != nil {
		return err
	}

	return e.state.Put(state.Torrent{
		InfoHash:        md.InfoHash,
		Title:           title,
		SizeBytes:       sizeBytes,
		StorageLocation: storageLocation,
		AddedAt:         time.Now().UTC(),
	})
}

// RemoveTorrent stops seeding a held torrent, deletes its stored data, and
// drops it from keep-at's persisted state. Used both for swaps (displacing a
// lower-priority torrent) and for content removed from Academic Torrents.
func (e *Engine) RemoveTorrent(infoHash metainfo.Hash, storageLocation string) error {
	if t, ok := e.torrentClient.Torrent(infoHash); ok {
		t.Drop()
	}

	if store, ok := e.stores[storageLocation]; ok {
		if err := store.DeleteTorrent(infoHash); err != nil {
			return fmt.Errorf("engine: deleting stored data for %s: %w", infoHash.HexString(), err)
		}
	}

	_ = os.Remove(e.cachedTorrentPath(infoHash))

	return e.state.Remove(infoHash)
}

// peerObservation is one connected peer that self-identified as keep-at,
// with enough detail to feed both the anti-cascade decision (just the
// count) and network-wide stats (node identity and seed/leech state).
type peerObservation struct {
	nodeKey  string // best-effort node identity, see netstats.Tracker's doc comment
	complete bool   // whether this peer has every piece of this torrent
}

// keepAtPeers returns one observation per currently-connected peer on t
// that self-identifies as keep-at in the BitTorrent extended handshake.
// It's a lower bound: it only sees peers we're actually connected to, not
// the whole swarm, and a hostile peer could claim to be keep-at when it
// isn't. See buildinfo for why that's an accepted tradeoff.
func keepAtPeers(t *torrent.Torrent) []peerObservation {
	var out []peerObservation
	totalPieces := uint64(t.NumPieces())

	for _, pc := range t.PeerConns() {
		name, _ := pc.PeerClientName.Load().(string)
		if len(name) < len(buildinfo.ClientName) || name[:len(buildinfo.ClientName)] != buildinfo.ClientName {
			continue
		}

		nodeKey := pc.RemoteAddr.String()
		if host, _, err := net.SplitHostPort(nodeKey); err == nil {
			nodeKey = host
		}

		complete := totalPieces > 0 && pc.PeerPieces().GetCardinality() >= totalPieces
		out = append(out, peerObservation{nodeKey: nodeKey, complete: complete})
	}
	return out
}
