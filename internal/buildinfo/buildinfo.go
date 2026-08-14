// Package buildinfo holds identifiers that keep-at reports about itself,
// both to the BitTorrent swarm (so other keep-at nodes can recognize each
// other) and to operators (so `keep-at status` and self-update know what's
// running).
package buildinfo

import "strings"

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

// Role names appended to keep-at's advertised client identity. The main
// client actually seeds held torrents, while the network-status census's
// probe client (see engine's RunCensus) only joins swarms briefly to count
// other keep-at nodes and never uploads or downloads. Distinguishing them
// in the client string means a node that's merely probing a swarm doesn't
// get counted as a keep-at seeder by other keep-at nodes' anti-cascade
// logic or by network-status.
const (
	RoleSeeder  = "seeder"
	RoleScraper = "scraper"
)

// PeerIDPrefix is the Azureus-style peer ID prefix keep-at identifies
// itself with at the BitTorrent wire protocol level, independent of the
// extended handshake string above.
const PeerIDPrefix = "-KA0100-"

// UserAgent is what keep-at sends on HTTP requests to academictorrents.com,
// so AT operators can identify keep-at traffic in their logs if something
// needs investigating.
func UserAgent() string {
	return "keep-at/" + Version + " (+https://github.com/tweedge/keep-at)"
}

// SeederUserAgent is the User-Agent sent on HTTP tracker announces by the
// main (seeding) torrent client. AT's tracker records this header on
// announces and shows it on each torrent's Technical page, so it's what
// makes a keep-at seeder show up as "keep-at" there rather than the torrent
// library's own default client name.
func SeederUserAgent() string {
	return "keep-at/" + Version + " (seeder) (+https://github.com/tweedge/keep-at)"
}

// ScraperUserAgent is the User-Agent sent on HTTP tracker announces by the
// probe (scraper) torrent client, and on keep-at's own tracker scrape
// requests. Distinct from SeederUserAgent so AT's logs and other keep-at
// nodes can tell a node that's only probing a swarm from one actually
// seeding it.
func ScraperUserAgent() string {
	return "keep-at/" + Version + " (scraper) (+https://github.com/tweedge/keep-at)"
}

// ExtendedHandshakeVersion is the full string sent in the BEP 10 extended
// handshake by the main (seeding) client, combining the client name other
// keep-at nodes match on with the version and role for diagnostics.
func ExtendedHandshakeVersion() string {
	return roleClientVersion(RoleSeeder)
}

// ScraperExtendedHandshakeVersion is the BEP 10 extended handshake "v"
// string for the probe (scraper) client, so other keep-at nodes can tell it
// apart from a real seeder.
func ScraperExtendedHandshakeVersion() string {
	return roleClientVersion(RoleScraper)
}

func roleClientVersion(role string) string {
	return ClientName + "/" + Version + " (" + role + ")"
}

// IsKeepAtSeeder reports whether a peer's advertised client string (the BEP
// 10 extended handshake "v" field) identifies it as a keep-at node that is
// actually seeding, as opposed to merely probing a swarm. A peer counts as a
// keep-at seeder if its name starts with ClientName and it does NOT carry
// the scraper role. The (seeder) role suffix is not required to match so
// that older keep-at versions, which advertised the bare "keep-at/version"
// string, still count as seeders.
func IsKeepAtSeeder(clientName string) bool {
	if len(clientName) < len(ClientName) || clientName[:len(ClientName)] != ClientName {
		return false
	}
	return !strings.Contains(clientName, "("+RoleScraper+")")
}
