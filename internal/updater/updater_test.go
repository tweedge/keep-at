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
	defer func() { _ = origReleasesURL }()

	version, err := applyFromURL(server.Client(), "test-agent", execPath, server.URL+"/releases/latest")
	if err != nil {
		t.Fatalf("applyFromURL: %v", err)
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

	if _, err := applyFromURL(server.Client(), "test-agent", execPath, server.URL+"/releases/latest"); err == nil {
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
