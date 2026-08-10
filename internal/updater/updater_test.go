package updater

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

func buildFakeReleaseAsset(t *testing.T, binaryContent []byte) []byte {
	t.Helper()
	var buf bytes.Buffer
	gz := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gz)

	if err := tw.WriteHeader(&tar.Header{Name: "keep-at", Mode: 0o755, Size: int64(len(binaryContent))}); err != nil {
		t.Fatalf("writing tar header: %v", err)
	}
	if _, err := tw.Write(binaryContent); err != nil {
		t.Fatalf("writing tar content: %v", err)
	}
	if err := tw.Close(); err != nil {
		t.Fatalf("closing tar writer: %v", err)
	}
	if err := gz.Close(); err != nil {
		t.Fatalf("closing gzip writer: %v", err)
	}
	return buf.Bytes()
}

func TestApplyDownloadsAndReplacesBinary(t *testing.T) {
	newContent := []byte("new keep-at binary contents")
	assetBytes := buildFakeReleaseAsset(t, newContent)
	assetName := fmt.Sprintf("keep-at_%s_%s.tar.gz", runtime.GOOS, runtime.GOARCH)

	mux := http.NewServeMux()
	server := httptest.NewServer(mux)
	defer server.Close()

	mux.HandleFunc("/releases/latest", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, `{"tag_name": "v1.2.3", "assets": [{"name": %q, "browser_download_url": %q}]}`,
			assetName, server.URL+"/download/"+assetName)
	})
	mux.HandleFunc("/download/"+assetName, func(w http.ResponseWriter, r *http.Request) {
		w.Write(assetBytes)
	})

	dir := t.TempDir()
	execPath := filepath.Join(dir, "keep-at")
	if err := os.WriteFile(execPath, []byte("old binary contents"), 0o755); err != nil {
		t.Fatalf("seeding old binary: %v", err)
	}

	origReleasesURL := ReleasesURL
	ReleasesURL = server.URL + "/releases/latest"
	origReleasesListURL := ReleasesListURL
	ReleasesListURL = server.URL + "/releases"
	defer func() {
		ReleasesURL = origReleasesURL
		ReleasesListURL = origReleasesListURL
	}()

	version, err := Apply(server.Client(), "test-agent", execPath, false)
	if err != nil {
		t.Fatalf("Apply: %v", err)
	}
	if version != "v1.2.3" {
		t.Errorf("version = %q, want v1.2.3", version)
	}

	got, err := os.ReadFile(execPath)
	if err != nil {
		t.Fatalf("reading updated binary: %v", err)
	}
	if !bytes.Equal(got, newContent) {
		t.Errorf("binary content = %q, want %q", got, newContent)
	}

	info, err := os.Stat(execPath)
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if info.Mode().Perm() != 0o755 {
		t.Errorf("mode = %v, want 0755", info.Mode().Perm())
	}
}

func TestApplyBetaUsesPrereleaseWhenAllowed(t *testing.T) {
	newContent := []byte("beta binary contents")
	assetBytes := buildFakeReleaseAsset(t, newContent)
	assetName := fmt.Sprintf("keep-at_%s_%s.tar.gz", runtime.GOOS, runtime.GOARCH)

	mux := http.NewServeMux()
	server := httptest.NewServer(mux)
	defer server.Close()

	// /releases lists newest first. A beta release on top, stable below it.
	mux.HandleFunc("/releases", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, `[
			{"tag_name": "v0.7.1-beta", "draft": false, "assets": [{"name": %q, "browser_download_url": %q}]},
			{"tag_name": "v0.7.0", "draft": false, "assets": []}
		]`, assetName, server.URL+"/download/"+assetName)
	})
	mux.HandleFunc("/download/"+assetName, func(w http.ResponseWriter, r *http.Request) {
		w.Write(assetBytes)
	})

	dir := t.TempDir()
	execPath := filepath.Join(dir, "keep-at")
	if err := os.WriteFile(execPath, []byte("old"), 0o755); err != nil {
		t.Fatalf("seeding old binary: %v", err)
	}

	origReleasesURL := ReleasesURL
	ReleasesURL = server.URL + "/releases/latest"
	origReleasesListURL := ReleasesListURL
	ReleasesListURL = server.URL + "/releases"
	defer func() {
		ReleasesURL = origReleasesURL
		ReleasesListURL = origReleasesListURL
	}()

	// With includeBeta true, the beta release at the top of the list wins.
	version, err := Apply(server.Client(), "test-agent", execPath, true)
	if err != nil {
		t.Fatalf("Apply(beta): %v", err)
	}
	if version != "v0.7.1-beta" {
		t.Errorf("beta version = %q, want v0.7.1-beta", version)
	}
	got, err := os.ReadFile(execPath)
	if err != nil {
		t.Fatalf("reading updated binary: %v", err)
	}
	if !bytes.Equal(got, newContent) {
		t.Errorf("binary content = %q, want %q", got, newContent)
	}
}

func TestApplyBetaSkipsDrafts(t *testing.T) {
	mux := http.NewServeMux()
	server := httptest.NewServer(mux)
	defer server.Close()

	mux.HandleFunc("/releases", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, `[
			{"tag_name": "v0.7.1-beta", "draft": true, "assets": []},
			{"tag_name": "v0.7.0", "draft": false, "assets": []}
		]`)
	})

	dir := t.TempDir()
	execPath := filepath.Join(dir, "keep-at")
	if err := os.WriteFile(execPath, []byte("old"), 0o755); err != nil {
		t.Fatalf("seeding old binary: %v", err)
	}

	origReleasesListURL := ReleasesListURL
	ReleasesListURL = server.URL + "/releases"
	defer func() { ReleasesListURL = origReleasesListURL }()

	// A draft is skipped even with includeBeta true; the stable release
	// below it is selected but has no matching asset, so Apply errors rather
	// than silently updating to nothing.
	if _, err := Apply(server.Client(), "test-agent", execPath, true); err == nil {
		t.Fatalf("expected an error when the only non-draft release lacks an asset")
	}
}

func TestApplyErrorsWhenAssetMissing(t *testing.T) {
	mux := http.NewServeMux()
	server := httptest.NewServer(mux)
	defer server.Close()

	mux.HandleFunc("/releases/latest", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, `{"tag_name": "v1.2.3", "assets": []}`)
	})

	dir := t.TempDir()
	execPath := filepath.Join(dir, "keep-at")
	if err := os.WriteFile(execPath, []byte("old"), 0o755); err != nil {
		t.Fatalf("seeding old binary: %v", err)
	}

	origReleasesURL := ReleasesURL
	ReleasesURL = server.URL + "/releases/latest"
	defer func() { ReleasesURL = origReleasesURL }()

	if _, err := Apply(server.Client(), "test-agent", execPath, false); err == nil {
		t.Fatalf("expected an error when no matching asset exists")
	}

	got, err := os.ReadFile(execPath)
	if err != nil {
		t.Fatalf("reading binary: %v", err)
	}
	if string(got) != "old" {
		t.Fatalf("binary should be untouched on failure, got %q", got)
	}
}
