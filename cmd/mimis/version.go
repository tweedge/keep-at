package main

import (
	"fmt"

	"github.com/tweedge/mimisbaeti/internal/buildinfo"
)

func cmdVersion() {
	fmt.Printf("mimis %s (%s)\n", buildinfo.Version, buildinfo.Commit)
}
