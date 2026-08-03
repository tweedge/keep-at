package service

import (
	"bytes"
	"strings"
	"testing"
)

func TestUnitTemplateRendersExpectedFields(t *testing.T) {
	var buf bytes.Buffer
	data := unitTemplateData{
		ExecStartLine: execStartLine("/usr/local/bin/keep-at", []string{"run", "--config", "/etc/keep-at/config.yaml"}),
		User:          "keep-at",
	}
	if err := unitTemplate.Execute(&buf, data); err != nil {
		t.Fatalf("executing template: %v", err)
	}

	out := buf.String()
	wantSubstrings := []string{
		"ExecStart=/usr/local/bin/keep-at run --config /etc/keep-at/config.yaml",
		"User=keep-at",
		"WantedBy=multi-user.target",
	}
	for _, want := range wantSubstrings {
		if !strings.Contains(out, want) {
			t.Errorf("rendered unit missing %q\nfull output:\n%s", want, out)
		}
	}
}

func TestExecStartLineQuotesArgsWithSpaces(t *testing.T) {
	got := execStartLine("/usr/local/bin/keep-at", []string{"run", "--storage", "/mnt/my disk/keep-at"})
	want := `/usr/local/bin/keep-at run --storage "/mnt/my disk/keep-at"`
	if got != want {
		t.Errorf("execStartLine() = %q, want %q", got, want)
	}
}
