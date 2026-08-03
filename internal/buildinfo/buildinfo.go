// Package buildinfo holds identifiers that mimis reports about itself, both
// to the BitTorrent swarm (so other mimis nodes can recognize each other) and
// to operators (so `mimis status` and self-update know what's running).
package buildinfo

// Version is the mimis release version. It's overwritten at build time via
// -ldflags "-X github.com/tweedge/mimisbaeti/internal/buildinfo.Version=...".
var Version = "dev"

// Commit is the git commit mimis was built from, set the same way as Version.
var Commit = "unknown"

// ClientName is what mimis advertises in the BitTorrent extended handshake
// (BEP 10) "v" field. Other mimis nodes look for this exact prefix to count
// how many of them are already seeding a given torrent, which feeds the
// anti-cascade swap logic. Changing this string changes what counts as a
// "mimis peer" to older or newer versions, so bump it deliberately.
const ClientName = "mimisbaeti"

// PeerIDPrefix is the Azureus-style peer ID prefix mimis identifies itself
// with at the BitTorrent wire protocol level, independent of the extended
// handshake string above.
const PeerIDPrefix = "-mB0100-"

// UserAgent is what mimis sends on HTTP requests to academictorrents.com and
// to HTTP trackers, so AT operators can identify mimis traffic in their logs
// if something needs investigating.
func UserAgent() string {
	return "mimisbaeti/" + Version + " (+https://github.com/tweedge/mimisbaeti)"
}

// ExtendedHandshakeVersion is the full string sent in the BEP 10 extended
// handshake, combining the client name other mimis nodes match on with the
// version for diagnostics.
func ExtendedHandshakeVersion() string {
	return ClientName + "/" + Version
}
