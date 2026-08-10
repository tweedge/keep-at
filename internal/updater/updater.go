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

// ReleasesURL is GitHub's API endpoint for the latest stable (non-prerelease)
// keep-at release. Release assets are expected to be named
// keep-at_<GOOS>_<GOARCH>.tar.gz, each containing a single "keep-at"
// binary - see the build scripts. It's a var (not const) so tests can point
// it at an httptest server.
var ReleasesURL = "https://api.github.com/repos/tweedge/keep-at/releases/latest"

// ReleasesListURL lists all releases, newest first, including prereleases.
// Used when the user opts into beta versions: the first non-draft release
// in the list may be a prerelease, which is exactly what a beta update wants.
// A var for the same testability reason as ReleasesURL.
var ReleasesListURL = "https://api.github.com/repos/tweedge/keep-at/releases"

type release struct {
	TagName string  `json:"tag_name"`
	Draft   bool    `json:"draft"`
	Assets  []asset `json:"assets"`
}

type asset struct {
	Name               string `json:"name"`
	BrowserDownloadURL string `json:"browser_download_url"`
}

// LatestVersion returns the tag name of the latest GitHub release.
// includeBeta selects whether prerelease (beta) releases are eligible.
func LatestVersion(client *http.Client, userAgent string, includeBeta bool) (string, error) {
	rel, err := fetchLatestRelease(client, userAgent, includeBeta)
	if err != nil {
		return "", err
	}
	return rel.TagName, nil
}

// Apply downloads the release asset matching the current platform and
// replaces the binary at currentExecPath with it, preserving its file
// mode. includeBeta selects whether prerelease (beta) releases are
// eligible; the default (false) only considers stable releases. It
// downloads to a temp file first and only swaps the executable in
// afterward, so a failed or interrupted update leaves the existing binary
// untouched.
func Apply(client *http.Client, userAgent, currentExecPath string, includeBeta bool) (newVersion string, err error) {
	rel, err := fetchLatestRelease(client, userAgent, includeBeta)
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

// fetchLatestRelease returns the release keep-at should update to. With
// includeBeta false it uses GitHub's /releases/latest endpoint, which
// already excludes prereleases. With includeBeta true it lists all releases
// and takes the newest non-draft one, which may be a prerelease.
func fetchLatestRelease(client *http.Client, userAgent string, includeBeta bool) (*release, error) {
	if !includeBeta {
		return fetchReleaseFromURL(client, userAgent, ReleasesURL)
	}

	req, err := http.NewRequest(http.MethodGet, ReleasesListURL, nil)
	if err != nil {
		return nil, fmt.Errorf("updater: building request: %w", err)
	}
	req.Header.Set("User-Agent", userAgent)
	req.Header.Set("Accept", "application/vnd.github+json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("updater: fetching releases: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("updater: fetching releases returned %s", resp.Status)
	}

	var releases []release
	if err := json.NewDecoder(resp.Body).Decode(&releases); err != nil {
		return nil, fmt.Errorf("updater: parsing release metadata: %w", err)
	}
	for i := range releases {
		if releases[i].Draft {
			continue
		}
		return &releases[i], nil
	}
	return nil, fmt.Errorf("updater: no releases found")
}

// fetchReleaseFromURL fetches a single release object from releasesURL.
func fetchReleaseFromURL(client *http.Client, userAgent, releasesURL string) (*release, error) {
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
