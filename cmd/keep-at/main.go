// Command keep-at is the entry point for the keep-at smart node: a
// standalone daemon that scans Academic Torrents and seeds whatever's most
// urgently under-seeded, within whatever storage the operator has given it.
package main

import (
	"fmt"
	"os"
)

func main() {
	if len(os.Args) < 2 {
		printUsage()
		os.Exit(2)
	}

	args := os.Args[2:]
	var err error

	switch os.Args[1] {
	case "run":
		err = cmdRun(args)
	case "start":
		err = cmdStart(args)
	case "stop":
		err = cmdStop(args)
	case "status":
		err = cmdStatus(args)
	case "service":
		err = cmdService(args)
	case "network-status":
		err = cmdNetworkStatus(args)
	case "hosted-torrents":
		err = cmdHostedTorrents(args)
	case "self-update":
		err = cmdSelfUpdate(args)
	case "version":
		cmdVersion()
	case "-h", "--help", "help":
		printUsage()
	default:
		fmt.Fprintf(os.Stderr, "keep-at: unknown command %q\n\n", os.Args[1])
		printUsage()
		os.Exit(2)
	}

	if err != nil {
		fmt.Fprintf(os.Stderr, "keep-at: %v\n", err)
		os.Exit(1)
	}
}

func printUsage() {
	fmt.Fprint(os.Stderr, `keep-at - a smart node that seeds Academic Torrents

Usage:
  keep-at run [--config PATH]              run in the foreground
  keep-at start [--config PATH] [--foreground]
                                          start as a background process
  keep-at stop [--config PATH]             stop the background process
  keep-at status [--config PATH]           report whether keep-at is running
  keep-at service install [flags]          install a systemd service (Linux, root)
  keep-at service uninstall                remove the systemd service (Linux, root)
  keep-at network-status [--config PATH]   report on other keep-at nodes seen while scanning
  keep-at hosted-torrents [--config PATH]  list torrents this host holds and seeds
  keep-at self-update                      update to the latest release
  keep-at version                          print the version and exit

Run/start take every config setting as a flag (run --help for the full
list); a config file is optional and only needed for multiple storage
locations. At minimum: keep-at run --storage-limit 500G
`)
}
