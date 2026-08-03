// Package updater implements keep-at's self-update command: check GitHub
// releases for tweedge/keep-at, download the asset matching the current
// OS/architecture, and replace the running binary.
package updater

import (
	"archive/tar"
	"compress/gzip"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
)

// ReleasesURL is GitHub's API endpoint for the latest keep-at release.
// Release assets are expected to be named
// keep-at_<GOOS>_<GOARCH>.tar.gz, each containing a single "keep-at"
// binary - see the build scripts.
const ReleasesURL = "https://api.github.com/repos/tweedge/keep-at/releases/latest"

type release struct {
	TagName string  `json:"tag_name"`
	Assets  []asset `json:"assets"`
}

type asset struct {
	Name               string `json:"name"`
	BrowserDownloadURL string `json:"browser_download_url"`
}

// LatestVersion returns the tag name of the latest GitHub release.
func LatestVersion(client *http.Client, userAgent string) (string, error) {
	rel, err := fetchLatestRelease(client, userAgent, ReleasesURL)
	if err != nil {
		return "", err
	}
	return rel.TagName, nil
}

// Apply downloads the release asset matching the current platform and
// replaces the binary at currentExecPath with it, preserving its file
// mode. It downloads to a temp file first and only swaps the executable in
// afterward, so a failed or interrupted update leaves the existing binary
// untouched.
func Apply(client *http.Client, userAgent, currentExecPath string) (newVersion string, err error) {
	return applyFromURL(client, userAgent, currentExecPath, ReleasesURL)
}

// applyFromURL is Apply with the releases endpoint overridable, so tests
// can point it at an httptest server instead of the real GitHub API.
func applyFromURL(client *http.Client, userAgent, currentExecPath, releasesURL string) (newVersion string, err error) {
	rel, err := fetchLatestRelease(client, userAgent, releasesURL)
	if err != nil {
		return "", err
	}

	assetName := fmt.Sprintf("keep-at_%s_%s.tar.gz", runtime.GOOS, runtime.GOARCH)
	var downloadURL string
	for _, a := range rel.Assets {
		if a.Name == assetName {
			downloadURL = a.BrowserDownloadURL
			break
		}
	}
	if downloadURL == "" {
		return "", fmt.Errorf("updater: no release asset named %s in %s", assetName, rel.TagName)
	}

	binary, err := downloadBinary(client, userAgent, downloadURL)
	if err != nil {
		return "", err
	}

	if err := replaceExecutable(currentExecPath, binary); err != nil {
		return "", err
	}

	return rel.TagName, nil
}

func fetchLatestRelease(client *http.Client, userAgent, releasesURL string) (*release, error) {
	req, err := http.NewRequest(http.MethodGet, releasesURL, nil)
	if err != nil {
		return nil, fmt.Errorf("updater: building request: %w", err)
	}
	req.Header.Set("User-Agent", userAgent)
	req.Header.Set("Accept", "application/vnd.github+json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("updater: fetching latest release: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("updater: fetching latest release returned %s", resp.Status)
	}

	var rel release
	if err := json.NewDecoder(resp.Body).Decode(&rel); err != nil {
		return nil, fmt.Errorf("updater: parsing release metadata: %w", err)
	}
	return &rel, nil
}

// downloadBinary fetches a .tar.gz release asset and extracts the single
// "keep-at" binary from it into memory.
func downloadBinary(client *http.Client, userAgent, url string) ([]byte, error) {
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("updater: building download request: %w", err)
	}
	req.Header.Set("User-Agent", userAgent)

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("updater: downloading %s: %w", url, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("updater: downloading %s returned %s", url, resp.Status)
	}

	gz, err := gzip.NewReader(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("updater: opening gzip stream: %w", err)
	}
	defer gz.Close()

	tr := tar.NewReader(gz)
	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			return nil, fmt.Errorf("updater: %s did not contain a keep-at binary", url)
		}
		if err != nil {
			return nil, fmt.Errorf("updater: reading tar archive: %w", err)
		}
		base := filepath.Base(hdr.Name)
		if base != "keep-at" && base != "keep-at.exe" {
			continue
		}
		return io.ReadAll(tr)
	}
}

// replaceExecutable atomically swaps targetPath's contents for newBinary,
// preserving targetPath's existing file permissions.
func replaceExecutable(targetPath string, newBinary []byte) error {
	info, err := os.Stat(targetPath)
	mode := os.FileMode(0o755)
	if err == nil {
		mode = info.Mode()
	}

	dir := filepath.Dir(targetPath)
	tmp, err := os.CreateTemp(dir, ".keep-at-update-*")
	if err != nil {
		return fmt.Errorf("updater: creating temp file in %s: %w", dir, err)
	}
	tmpPath := tmp.Name()
	defer os.Remove(tmpPath) // no-op once the rename below succeeds

	if _, err := tmp.Write(newBinary); err != nil {
		tmp.Close()
		return fmt.Errorf("updater: writing new binary: %w", err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("updater: finalizing new binary: %w", err)
	}
	if err := os.Chmod(tmpPath, mode); err != nil {
		return fmt.Errorf("updater: setting permissions: %w", err)
	}
	if err := os.Rename(tmpPath, targetPath); err != nil {
		return fmt.Errorf("updater: replacing %s: %w", targetPath, err)
	}
	return nil
}
