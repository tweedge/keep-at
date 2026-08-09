package filter

import "time"

// AgeEligible reports whether a torrent created at createdAt has existed
// long enough (minAge, 7 days by default) to be considered moderated.
//
// A zero minAge disables the moderation-age gate entirely: keep-at considers
// every torrent eligible regardless of age. That includes torrents with no
// posted creation date, which otherwise have no age to judge - when the
// operator has explicitly turned the age gate off, there's no reason to
// withhold them.
//
// With a positive minAge, a zero createdAt means keep-at couldn't determine
// an age - in that case, keep-at is conservative and treats it as not yet
// eligible, since the whole point of the delay is not downloading content
// nobody's had a chance to review yet.
func AgeEligible(createdAt time.Time, minAge time.Duration, now time.Time) bool {
	if minAge <= 0 {
		return true
	}
	if createdAt.IsZero() {
		return false
	}
	return now.Sub(createdAt) >= minAge
}
