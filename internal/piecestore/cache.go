package piecestore

import (
	"container/list"
	"sync"
)

// decompressCache holds recently-decompressed piece bytes so that serving
// several small chunk requests against the same piece (which is how peers
// actually ask for data - 16KB at a time) doesn't re-run gzip decompression
// for every single request. It's bounded by total bytes, not entry count,
// since piece sizes vary a lot across the Academic Torrents catalog.
type decompressCache struct {
	mu       sync.Mutex
	maxBytes int64
	curBytes int64
	order    *list.List // front = most recently used
	entries  map[string]*list.Element
}

type cacheEntry struct {
	key  string
	data []byte
}

func newDecompressCache(maxBytes int64) *decompressCache {
	return &decompressCache{
		maxBytes: maxBytes,
		order:    list.New(),
		entries:  make(map[string]*list.Element),
	}
}

func (c *decompressCache) get(key string) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	elem, ok := c.entries[key]
	if !ok {
		return nil, false
	}
	c.order.MoveToFront(elem)
	return elem.Value.(*cacheEntry).data, true
}

func (c *decompressCache) put(key string, data []byte) {
	if c.maxBytes <= 0 {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()

	if elem, ok := c.entries[key]; ok {
		c.curBytes -= int64(len(elem.Value.(*cacheEntry).data))
		c.order.Remove(elem)
		delete(c.entries, key)
	}

	c.curBytes += int64(len(data))
	elem := c.order.PushFront(&cacheEntry{key: key, data: data})
	c.entries[key] = elem

	for c.curBytes > c.maxBytes && c.order.Len() > 0 {
		back := c.order.Back()
		entry := back.Value.(*cacheEntry)
		c.curBytes -= int64(len(entry.data))
		c.order.Remove(back)
		delete(c.entries, entry.key)
	}
}

func (c *decompressCache) invalidate(key string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if elem, ok := c.entries[key]; ok {
		c.curBytes -= int64(len(elem.Value.(*cacheEntry).data))
		c.order.Remove(elem)
		delete(c.entries, key)
	}
}
