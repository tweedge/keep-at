package config

import (
	"fmt"
	"strconv"
	"strings"
)

// ByteSize is a storage quantity, parsed from strings like "500G" or "2T".
// Suffixes are binary (1 KiB = 1024 bytes) since that's what disks and
// filesystems actually report; keep-at just uses the plan's letters (M, G, T,
// P) rather than the pedantically correct MiB/GiB/TiB/PiB ones.
type ByteSize int64

const (
	byteSizeKilo ByteSize = 1 << 10
	byteSizeMega          = byteSizeKilo << 10
	byteSizeGiga          = byteSizeMega << 10
	byteSizeTera          = byteSizeGiga << 10
	byteSizePeta          = byteSizeTera << 10
)

var byteSizeSuffixes = []struct {
	suffix string
	unit   ByteSize
}{
	{"P", byteSizePeta},
	{"T", byteSizeTera},
	{"G", byteSizeGiga},
	{"M", byteSizeMega},
}

// ParseByteSize parses a fixed integer followed by M, G, T, or P (case
// insensitive) into a byte count, e.g. "500G" -> 500 * 1024^3. A bare
// integer with no suffix is taken as a plain byte count, which is how "0"
// (meaning unlimited for rate limits) and small exact values are expressed.
func ParseByteSize(s string) (ByteSize, error) {
	trimmed := strings.TrimSpace(s)
	if trimmed == "" {
		return 0, fmt.Errorf("byte size is empty")
	}

	upper := strings.ToUpper(trimmed)
	for _, candidate := range byteSizeSuffixes {
		if strings.HasSuffix(upper, candidate.suffix) {
			numeric := strings.TrimSpace(upper[:len(upper)-len(candidate.suffix)])
			value, err := strconv.ParseInt(numeric, 10, 64)
			if err != nil {
				return 0, fmt.Errorf("byte size %q: %w", s, err)
			}
			if value < 0 {
				return 0, fmt.Errorf("byte size %q: must not be negative", s)
			}
			return ByteSize(value) * candidate.unit, nil
		}
	}

	// No suffix: treat the whole thing as a plain byte count (e.g. "0" or
	// "4096"). A numeric suffix-less value with a trailing letter that isn't
	// M/G/T/P still errors out below, since ParseInt will reject it.
	value, err := strconv.ParseInt(upper, 10, 64)
	if err != nil {
		return 0, fmt.Errorf("byte size %q: must end in M, G, T, or P (or be a plain byte count)", s)
	}
	if value < 0 {
		return 0, fmt.Errorf("byte size %q: must not be negative", s)
	}
	return ByteSize(value), nil
}

func (b ByteSize) String() string {
	switch {
	case b == 0:
		return "0"
	case b >= byteSizePeta && b%byteSizePeta == 0:
		return fmt.Sprintf("%dP", b/byteSizePeta)
	case b >= byteSizeTera && b%byteSizeTera == 0:
		return fmt.Sprintf("%dT", b/byteSizeTera)
	case b >= byteSizeGiga && b%byteSizeGiga == 0:
		return fmt.Sprintf("%dG", b/byteSizeGiga)
	case b >= byteSizeMega && b%byteSizeMega == 0:
		return fmt.Sprintf("%dM", b/byteSizeMega)
	default:
		return fmt.Sprintf("%dM", (b+byteSizeMega-1)/byteSizeMega)
	}
}

func (b *ByteSize) UnmarshalYAML(unmarshal func(interface{}) error) error {
	var raw string
	if err := unmarshal(&raw); err != nil {
		return err
	}
	parsed, err := ParseByteSize(raw)
	if err != nil {
		return err
	}
	*b = parsed
	return nil
}

func (b ByteSize) MarshalYAML() (interface{}, error) {
	return b.String(), nil
}
