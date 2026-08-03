package config

import (
	"os"
	"path/filepath"
	"runtime"
)

// platformAppDir returns the conventional per-user application data
// directory for appName on the current OS:
//
//   - Linux: $XDG_DATA_HOME/appName, falling back to ~/.local/share/appName
//   - macOS: ~/Library/Application Support/appName
//   - Windows: %LOCALAPPDATA%\appName, falling back to %APPDATA%\appName
//   - anything else: ~/.appName
//
// If the home directory can't be determined at all, it falls back to
// /var/lib/appName so keep-at still has somewhere to write.
func platformAppDir(appName string) string {
	home, homeErr := os.UserHomeDir()

	switch runtime.GOOS {
	case "linux":
		if xdg := os.Getenv("XDG_DATA_HOME"); xdg != "" {
			return filepath.Join(xdg, appName)
		}
		if homeErr == nil && home != "" {
			return filepath.Join(home, ".local", "share", appName)
		}
	case "darwin":
		if homeErr == nil && home != "" {
			return filepath.Join(home, "Library", "Application Support", appName)
		}
	case "windows":
		if local := os.Getenv("LOCALAPPDATA"); local != "" {
			return filepath.Join(local, appName)
		}
		if roaming := os.Getenv("APPDATA"); roaming != "" {
			return filepath.Join(roaming, appName)
		}
	default:
		if homeErr == nil && home != "" {
			return filepath.Join(home, "."+appName)
		}
	}

	if homeErr == nil && home != "" {
		return filepath.Join(home, "."+appName)
	}
	return filepath.Join("/var/lib", appName)
}
