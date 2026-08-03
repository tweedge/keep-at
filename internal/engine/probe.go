package engine

import (
	"context"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
)

// probePollInterval and probeMaxPeers bound how long mimis waits to gather
// peer connections when probing a candidate torrent's swarm for other
// mimis nodes. Scans are already expected to take a while (see PLAN.md),
// but there's no reason to wait the full timeout once enough peers have
// answered to make a reasonable estimate.
const (
	probePollInterval   = 500 * time.Millisecond
	probeMaxPeers       = 8
	defaultProbeTimeout = 10 * time.Second
)

// probeMimisPeerCount briefly joins a candidate's swarm - without
// allowing any actual data transfer - just to see how many connected peers
// self-identify as mimis nodes. This is the only way to answer "how many
// mimis nodes are already on this torrent" for a torrent mimis isn't
// downloading yet: that information only exists on the wire, in each
// peer's BitTorrent extended handshake, not in any catalog or tracker
// response.
//
// The torrent is always dropped again before returning; callers that
// decide to proceed re-add it properly (with real storage and transfer
// allowed) via AddCandidate.
func (e *Engine) probeMimisPeerCount(ctx context.Context, mi *metainfo.MetaInfo, timeout time.Duration) (int, error) {
	spec := torrent.TorrentSpecFromMetaInfo(mi)
	spec.Storage = e.probeStore
	spec.DisallowDataDownload = true
	spec.DisallowDataUpload = true

	t, _, err := e.torrentClient.AddTorrentSpec(spec)
	if err != nil {
		return 0, err
	}
	defer t.Drop()

	deadline := time.Now().Add(timeout)
	ticker := time.NewTicker(probePollInterval)
	defer ticker.Stop()

	for {
		if count := mimisPeerCount(t); count > 0 || len(t.PeerConns()) >= probeMaxPeers {
			return count, nil
		}
		if time.Now().After(deadline) {
			return mimisPeerCount(t), nil
		}
		select {
		case <-ctx.Done():
			return mimisPeerCount(t), ctx.Err()
		case <-ticker.C:
		}
	}
}
