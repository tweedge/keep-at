# keep-at release notes

## v0.5.2 - lighter scans, usable on 1GB-RAM devices

Full-catalog scans no longer hold every candidate's parsed `.torrent` metadata in memory. The scan previously kept the complete metadata - including every torrent's piece-hash arrays, which scale with the library's total size - resident for the whole scan, pushing a first scan past 3GB of RAM even on a machine with a modest library. Now:

- **Candidates stay lightweight during the scan.** Only the facts ranking needs (title, infohash, size, scraped swarm counts) are kept in memory. The full `.torrent` metadata is cached to `torrent-cache/` during evaluation and re-read from disk only when keep-at actually acts on a candidate. Scan memory is proportional to the number of candidates, not the size of the library.
- **Ranking work is bounded.** Acting happens once per batch of concurrent evaluations (plus a final flush) instead of re-ranking the whole catalog on every single candidate arrival.
- **Scrape progress logs less often.** The "scrape in progress" line now prints every 5 minutes instead of every 2.

The result: a full-catalog scan's memory footprint stays modest even on a 1GB-RAM Raspberry Pi-class device, while seeding still starts within minutes.
