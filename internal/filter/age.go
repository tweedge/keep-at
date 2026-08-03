package filter

import "time"

// AgeEligible reports whether a torrent created at createdAt has existed
// long enough (minAge, 7 days by default) to be considered moderated. A
// zero createdAt means mimis couldn't determine an age - in that case,
// mimis is conservative and treats it as not yet eligible, since the whole
// point of the delay is not downloading content nobody's had a chance to
// review yet.
func AgeEligible(createdAt time.Time, minAge time.Duration, now time.Time) bool {
	if createdAt.IsZero() {
		return false
	}
	return now.Sub(createdAt) >= minAge
}
