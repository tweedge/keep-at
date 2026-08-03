package atcatalog

import (
	"strings"
	"testing"
)

const sampleXML = `<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:academictorrents="https://academictorrents.com" version="2.0">
<channel>
<title>Academic Torrents</title>
<item>
<title>Good Item</title>
<category>Dataset</category>
<infohash>30ac2ef27829b1b5a7d0644097f55f335ca5241b</infohash>
<guid>https://academictorrents.com/details/30ac2ef27829b1b5a7d0644097f55f335ca5241b</guid>
<link>https://academictorrents.com/details/30ac2ef27829b1b5a7d0644097f55f335ca5241b</link>
<description>A perfectly good dataset.</description>
<size>12345</size>
</item><item>
<title>Bad Infohash Item</title>
<category>Dataset</category>
<infohash>not-a-valid-hash</infohash>
<guid>https://academictorrents.com/details/not-a-valid-hash</guid>
<link>https://academictorrents.com/details/not-a-valid-hash</link>
<description/>
<size>1</size>
</item></channel>
</rss>`

func TestParseSkipsBadInfohashes(t *testing.T) {
	cat, err := Parse(strings.NewReader(sampleXML))
	if err != nil {
		t.Fatalf("Parse: %v", err)
	}
	if len(cat.Items) != 1 {
		t.Fatalf("expected 1 valid item, got %d", len(cat.Items))
	}
	item := cat.Items[0]
	if item.Title != "Good Item" {
		t.Fatalf("unexpected title: %q", item.Title)
	}
	if item.SizeBytes != 12345 {
		t.Fatalf("unexpected size: %d", item.SizeBytes)
	}
	if item.InfoHash.HexString() != "30ac2ef27829b1b5a7d0644097f55f335ca5241b" {
		t.Fatalf("unexpected infohash: %s", item.InfoHash.HexString())
	}
	if cat.FetchedAt.IsZero() {
		t.Fatalf("expected FetchedAt to be set")
	}
}
