// Command mimis is the entry point for the mimisbaeti smart node: a
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
	case "self-update":
		err = cmdSelfUpdate(args)
	case "version":
		cmdVersion()
	case "-h", "--help", "help":
		printUsage()
	default:
		fmt.Fprintf(os.Stderr, "mimis: unknown command %q\n\n", os.Args[1])
		printUsage()
		os.Exit(2)
	}

	if err != nil {
		fmt.Fprintf(os.Stderr, "mimis: %v\n", err)
		os.Exit(1)
	}
}

func printUsage() {
	fmt.Fprint(os.Stderr, `mimis - a smart node that seeds Academic Torrents

Usage:
  mimis run [--config PATH]              run in the foreground
  mimis start [--config PATH] [--foreground]
                                          start as a background process
  mimis stop [--config PATH]             stop the background process
  mimis status [--config PATH]           report whether mimis is running
  mimis service install [--config PATH]  install a systemd service (Linux, root)
  mimis service uninstall                remove the systemd service (Linux, root)
  mimis self-update                      update to the latest release
  mimis version                          print the version and exit
`)
}
