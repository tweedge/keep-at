// Package buildinfo holds identifiers that keep-at reports about itself,
// both to the BitTorrent swarm (so other keep-at nodes can recognize each
// other) and to operators (so `keep-at status` and self-update know what's
// running).
package buildinfo

// Version is the keep-at release version. It's overwritten at build time
// via -ldflags "-X github.com/tweedge/keep-at/internal/buildinfo.Version=...".
var Version = "dev"

// Commit is the git commit keep-at was built from, set the same way as
// Version.
var Commit = "unknown"

// ClientName is what keep-at advertises in the BitTorrent extended
// handshake (BEP 10) "v" field. Other keep-at nodes look for this exact
// prefix to count how many of them are already seeding a given torrent,
// which feeds the anti-cascade swap logic. Changing this string changes
// what counts as a "keep-at peer" to older or newer versions, so bump it
// deliberately.
const ClientName = "keep-at"

// PeerIDPrefix is the Azureus-style peer ID prefix keep-at identifies
// itself with at the BitTorrent wire protocol level, independent of the
// extended handshake string above.
const PeerIDPrefix = "-KA0100-"

// UserAgent is what keep-at sends on HTTP requests to academictorrents.com
// and to HTTP trackers, so AT operators can identify keep-at traffic in
// their logs if something needs investigating.
func UserAgent() string {
	return "keep-at/" + Version + " (+https://github.com/tweedge/keep-at)"
}

// ExtendedHandshakeVersion is the full string sent in the BEP 10 extended
// handshake, combining the client name other keep-at nodes match on with
// the version for diagnostics.
func ExtendedHandshakeVersion() string {
	return ClientName + "/" + Version
}
