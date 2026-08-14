package engine

import (
	"fmt"
	"net"
	"os"
	"sync/atomic"
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
//
// Re-adding a torrent is cheap in torrent count but expensive in data: the
// client walks every stored piece to determine completion state, so the time
// scales with how much data keep-at holds (verified: a node holding 1.6TB
// took ~17 minutes to resume, while 30 small torrents resume in well under a
// second). On a node holding a lot, startup would otherwise be a silent wait
// before "keep-at started" - so this logs an immediate "resuming held
// torrents" line and then one progress line per progressLogInterval (5
// minutes) naming what it's currently working on and how far through it is.
func (e *Engine) resumeHeldTorrents() error {
	heldTorrents := e.state.All()
	total := len(heldTorrents)
	startedAt := time.Now()

	var processed atomic.Int64
	var currentTitle atomic.Value
	stopProgress := make(chan struct{})
	progressStopped := make(chan struct{})
	go func() {
		defer close(progressStopped)
		ticker := time.NewTicker(progressLogInterval)
		defer ticker.Stop()
		for {
			select {
			case <-stopProgress:
				return
			case <-ticker.C:
				p := int(processed.Load())
				title, _ := currentTitle.Load().(string)
				e.logger.Info("resume in progress",
					"processed", p,
					"remaining", total-p,
					"total", total,
					"current", title,
					"elapsed", humanDuration(time.Since(startedAt)))
			}
		}
	}()
	defer func() {
		close(stopProgress)
		<-progressStopped
	}()

	e.logger.Info("resuming held torrents", "total", total)

	for _, held := range heldTorrents {
		currentTitle.Store(held.Title)
		store, ok := e.stores[held.StorageLocation]
		if !ok {
			e.logger.Warn("skipping resume: storage location no longer configured",
				"infohash", held.InfoHash.HexString(), "location", held.StorageLocation)
			processed.Add(1)
			continue
		}

		mi, err := metainfo.LoadFromFile(e.cachedTorrentPath(held.InfoHash))
		if err != nil {
			e.logger.Warn("skipping resume: could not load cached .torrent file",
				"infohash", held.InfoHash.HexString(), "err", err)
			processed.Add(1)
			continue
		}

		if _, err := e.addTorrentSpec(mi, store); err != nil {
			e.logger.Warn("skipping resume: could not add torrent to client",
				"infohash", held.InfoHash.HexString(), "err", err)
			processed.Add(1)
			continue
		}
		processed.Add(1)
	}
	return nil
}

// addTorrentSpec adds a parsed .torrent to the BitTorrent client, bound to
// a specific storage location, and starts downloading/seeding it. Already
// complete pieces (per the storage backend's own completion tracking) are
// not re-downloaded.
func (e *Engine) addTorrentSpec(mi *metainfo.MetaInfo, store *piecestore.Client) (*torrent.Torrent, error) {
	// TorrentSpecFromMetaInfoErr, not TorrentSpecFromMetaInfo: the latter
	// panics on incomplete metainfo, and a decade-plus of Academic
	// Torrents uploads means "malformed .torrent file" is an expected
	// case to handle, not a reason to crash.
	spec, err := torrent.TorrentSpecFromMetaInfoErr(mi)
	if err != nil {
		return nil, fmt.Errorf("engine: building torrent spec: %w", err)
	}
	spec.Storage = store
	// Restrict held torrents to Academic Torrents' own trackers (swapped
	// for the operator's per-user announce URL when a key is configured).
	// The anacrolix client re-announces to every tracker in a torrent's
	// spec on its own schedule, and AT's catalog entries list up to a
	// dozen mostly-dead third-party trackers each - so keeping them all
	// meant a node holding hundreds of torrents spent a huge fraction of
	// its CPU cycling announce timeouts against dead public trackers
	// (measured: the tracker-announce dispatcher alone was ~22% of a
	// 400-torrent node's CPU while idle-seeding). keep-at is an AT seeder:
	// AT's tracker and DHT are its peer discovery, and third-party
	// trackers get nothing back from this. The scrape path (scrapeSwarm)
	// was already AT-first; this makes the automatic announces AT-only too.
	spec.Trackers = atTrackersOnly(spec.Trackers, e.userAnnounceURL, e.userAnnounceIPv6URL)

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
// with enough detail to feed network-wide stats (node identity and
// seed/leech state) and to log the keep-at peer count as metadata.
type peerObservation struct {
	nodeKey  string // best-effort node identity, see netstats.Tracker's doc comment
	complete bool   // whether this peer has every piece of this torrent
}

// keepAtPeers returns one observation per currently-connected peer on t
// that self-identifies as a keep-at node actually seeding (not one merely
// probing the swarm). It's a lower bound: it only sees peers we're actually
// connected to, not the whole swarm, and a hostile peer could claim to be
// keep-at when it isn't. See buildinfo for why that's an accepted tradeoff.
func keepAtPeers(t *torrent.Torrent) []peerObservation {
	var out []peerObservation
	totalPieces := uint64(t.NumPieces())

	for _, pc := range t.PeerConns() {
		name, _ := pc.PeerClientName.Load().(string)
		if !buildinfo.IsKeepAtSeeder(name) {
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
