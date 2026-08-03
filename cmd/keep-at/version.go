package main

import (
	"fmt"

	"github.com/tweedge/keep-at/internal/buildinfo"
)

func cmdVersion() {
	fmt.Printf("keep-at %s (%s)\n", buildinfo.Version, buildinfo.Commit)
}
