package engine

import (
	"log/slog"
	"runtime/debug"
)

// safely runs fn and recovers from any panic within it, logging instead of
// crashing the whole scan. Academic Torrents' catalog spans well over a
// decade of uploads with wildly inconsistent metadata; the working
// assumption here is that any single torrent's data might be malformed in
// a way nobody has hit before, and one bad entry should never take down
// keep-at for everything else it's holding or evaluating.
//
// This can't catch every failure mode - a small number of synchronization
// bugs in anacrolix/torrent surface as an unrecoverable Go runtime fatal
// error rather than a normal panic (see engine.go's NoDHT comment for a
// real example), and those bypass recover() entirely by design. This is
// still worth having for the much larger set of ordinary panics (nil
// pointers, out-of-range indexes, and the like) that a decade of
// inconsistent metadata can plausibly trigger somewhere.
func safely(logger *slog.Logger, context string, fn func()) {
	defer func() {
		if r := recover(); r != nil {
			logger.Error("recovered from panic, continuing", "context", context, "panic", r, "stack", string(debug.Stack()))
		}
	}()
	fn()
}
