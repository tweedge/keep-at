// Package filter implements keep-at's pre-selection gates: keyword blocking and
// the moderation-age delay. Both answer the same kind of question - "should
// keep-at even consider this torrent?" - before the selector package gets to
// asking "how urgently does it need seeding?"
package filter

import "strings"

// KeywordBlocklist rejects catalog items whose title or description
// contains any blocked keyword, case-insensitively. Academic Torrents'
// database.xml only carries title, category, and description - there's no
// separate author/abstract/keywords field in the bulk catalog file, so
// that's what keep-at actually has to filter on without making a per-item API
// call for every single catalog entry (which would defeat the point of
// downloading the catalog in bulk in the first place).
type KeywordBlocklist struct {
	keywords []string
}

// NewKeywordBlocklist builds a blocklist from raw config strings. Empty and
// whitespace-only entries are ignored.
func NewKeywordBlocklist(keywords []string) KeywordBlocklist {
	cleaned := make([]string, 0, len(keywords))
	for _, kw := range keywords {
		trimmed := strings.ToLower(strings.TrimSpace(kw))
		if trimmed != "" {
			cleaned = append(cleaned, trimmed)
		}
	}
	return KeywordBlocklist{keywords: cleaned}
}

// Blocks reports whether title or description matches a blocked keyword,
// and if so, which one - useful for logging why keep-at skipped something.
func (b KeywordBlocklist) Blocks(title, description string) (blocked bool, matched string) {
	haystack := strings.ToLower(title + " " + description)
	for _, kw := range b.keywords {
		if strings.Contains(haystack, kw) {
			return true, kw
		}
	}
	return false, ""
}
