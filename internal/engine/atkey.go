package engine

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
)

// userAnnounceEndpoint is Academic Torrents' per-user announce resolution
// endpoint. Called with the operator's API key (uid/pass) as cookies, it
// returns the announce URL that carries that account's passkey, e.g.
// https://academictorrents.com/announce.php?passkey=abc123. This is the same
// mechanism AT's own smartnode tooling uses to attribute hosted torrents to
// an account; see the "Hosted by" attribution section of AT's docs. It's a
// var (not const) so tests can point it at a local stub.
var userAnnounceEndpoint = "https://academictorrents.com/apiv2/userannounce"

// atTrackerHosts are the only tracker hosts keep-at will ever send an
// operator's API key-derived announce URL to. Everything else - third-party
// trackers listed in .torrent files, other AT hosts, http:// (non-https)
// variants - must never see it.
var atTrackerHosts = map[string]bool{
	"academictorrents.com":      true,
	"ipv6.academictorrents.com": true,
}

// atAnnounceURL returns the per-user announce URL to use for a tracker URL,
// or "" if the tracker URL is not one keep-at is allowed to key (i.e. not an
// https URL on an AT tracker host).
func atAnnounceURL(announce string, userAnnounce, userAnnounceIPv6 string) string {
	u, err := url.Parse(announce)
	if err != nil || u.Scheme != "https" {
		return ""
	}
	switch u.Hostname() {
	case "academictorrents.com":
		return userAnnounce
	case "ipv6.academictorrents.com":
		return userAnnounceIPv6
	}
	return ""
}

// resolveUserAnnounce fetches the per-user announce URL for the given API key
// by calling AT's userannounce endpoint with the key sent as cookies (the
// key's format is literally "uid=N;pass=H", which is valid Cookie header
// syntax). Returns the announce URL and its ipv6.academictorrents.com
// variant. The returned URLs contain the account's passkey and must be
// treated as sensitive: never log them, never write them to disk.
func resolveUserAnnounce(ctx context.Context, client *http.Client, apiKey string) (string, string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, userAnnounceEndpoint, nil)
	if err != nil {
		return "", "", fmt.Errorf("engine: building userannounce request: %w", err)
	}
	req.Header.Set("Cookie", apiKey)

	resp, err := client.Do(req)
	if err != nil {
		return "", "", fmt.Errorf("engine: fetching user announce URL: %w", err)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", "", fmt.Errorf("engine: reading userannounce response: %w", err)
	}
	if resp.StatusCode != http.StatusOK {
		return "", "", fmt.Errorf("engine: userannounce endpoint returned %s", resp.Status)
	}

	var out struct {
		UserAnnounce string `json:"userannounce"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		return "", "", fmt.Errorf("engine: parsing userannounce response: %w", err)
	}
	if out.UserAnnounce == "" {
		return "", "", fmt.Errorf("engine: userannounce response had no announce URL")
	}

	// Derive the ipv6.academictorrents.com variant of the returned URL, so a
	// torrent listing the IPv6 tracker gets an equivalent keyed announce
	// instead of an unkeyed fallback. The main host and the passkey stay
	// exactly as AT returned them.
	ipv6 := ""
	if u, err := url.Parse(out.UserAnnounce); err == nil && u.Hostname() == "academictorrents.com" {
		u.Host = "ipv6.academictorrents.com"
		ipv6 = u.String()
	}

	return out.UserAnnounce, ipv6, nil
}

// keyedTrackers rewrites a tiered tracker list (as found on a TorrentSpec,
// i.e. mi.UpvertedAnnounceList) so that any Academic Torrents tracker is
// replaced with the operator's per-user announce URL. Third-party trackers
// and any non-matching URL are passed through untouched. If userAnnounce is
// empty (no key configured, or resolution failed), the list is returned
// unchanged.
func keyedTrackers(trackers [][]string, userAnnounce, userAnnounceIPv6 string) [][]string {
	if userAnnounce == "" {
		return trackers
	}
	out := make([][]string, 0, len(trackers))
	for _, tier := range trackers {
		newTier := make([]string, len(tier))
		for i, announce := range tier {
			if keyed := atAnnounceURL(announce, userAnnounce, userAnnounceIPv6); keyed != "" {
				newTier[i] = keyed
			} else {
				newTier[i] = announce
			}
		}
		out = append(out, newTier)
	}
	return out
}

// atTrackersOnly filters a tiered tracker list down to Academic Torrents'
// own trackers (swapped for the operator's per-user announce URL when one
// is configured), dropping every third-party tracker. Held torrents get
// this treatment (see addTorrentSpec): the underlying client re-announces
// to every tracker in a torrent's spec on its own schedule, and AT catalog
// entries list up to a dozen mostly-dead third-party trackers each - so
// keeping them meant a node holding hundreds of torrents burned CPU
// cycling announce timeouts against dead public trackers. keep-at is an AT
// seeder; AT's tracker and DHT are its peer discovery. The network-status
// census's probe client uses the same filtering.
//
// When the list contains no AT tracker at all (a non-AT catalog entry, a
// hand-built torrent, a test stub), it's returned unchanged - filtering to
// zero trackers would leave the torrent with no tracker-based peer
// discovery whatsoever, which is worse than keeping the third-party list.
func atTrackersOnly(trackers [][]string, userAnnounce, userAnnounceIPv6 string) [][]string {
	foundAT := false
	for _, tier := range trackers {
		for _, announce := range tier {
			if atAnnounceURL(announce, userAnnounce, userAnnounceIPv6) != "" {
				foundAT = true
				break
			}
		}
		if foundAT {
			break
		}
	}
	if !foundAT {
		return trackers
	}

	out := make([][]string, 0, len(trackers))
	for _, tier := range trackers {
		newTier := make([]string, 0, len(tier))
		for _, announce := range tier {
			if keyed := atAnnounceURL(announce, userAnnounce, userAnnounceIPv6); keyed != "" {
				newTier = append(newTier, keyed)
			}
		}
		if len(newTier) > 0 {
			out = append(out, newTier)
		}
	}
	return out
}
