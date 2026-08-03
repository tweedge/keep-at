package engine

import (
	"context"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
)

// probePollInterval and probeMaxPeers bound how long keep-at waits to gather
// peer connections when probing a candidate torrent's swarm for other
// keep-at nodes. Scans are already expected to take a while (see
// DESIGN.md), but there's no reason to wait the full timeout once enough
// peers have answered to make a reasonable estimate.
const (
	probePollInterval   = 500 * time.Millisecond
	probeMaxPeers       = 8
	defaultProbeTimeout = 10 * time.Second
)

// probeSwarm briefly joins a candidate's swarm - without allowing any
// actual data transfer - just to see which connected peers self-identify
// as keep-at nodes, and whether each one has the whole torrent already.
// This is the only way to answer "how many keep-at nodes are already on
// this torrent" (feeding the anti-cascade decision) and "are they seeding
// or still downloading it" (feeding network-status) for a torrent keep-at
// isn't itself downloading yet: that information only exists on the wire,
// in each peer's BitTorrent extended handshake and piece bitfield, not in
// any catalog or tracker response.
//
// The torrent is always dropped again before returning; callers that
// decide to proceed re-add it properly (with real storage and transfer
// allowed) via AddCandidate.
func (e *Engine) probeSwarm(ctx context.Context, mi *metainfo.MetaInfo, timeout time.Duration) ([]peerObservation, error) {
	spec := torrent.TorrentSpecFromMetaInfo(mi)
	spec.Storage = e.probeStore
	spec.DisallowDataDownload = true
	spec.DisallowDataUpload = true

	t, _, err := e.torrentClient.AddTorrentSpec(spec)
	if err != nil {
		return nil, err
	}
	defer t.Drop()

	deadline := time.Now().Add(timeout)
	ticker := time.NewTicker(probePollInterval)
	defer ticker.Stop()

	for {
		if observed := keepAtPeers(t); len(observed) > 0 || len(t.PeerConns()) >= probeMaxPeers {
			return observed, nil
		}
		if time.Now().After(deadline) {
			return keepAtPeers(t), nil
		}
		select {
		case <-ctx.Done():
			return keepAtPeers(t), ctx.Err()
		case <-ticker.C:
		}
	}
}
