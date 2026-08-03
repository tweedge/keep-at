package service

import (
	"bytes"
	"strings"
	"testing"
)

func TestUnitTemplateRendersExpectedFields(t *testing.T) {
	var buf bytes.Buffer
	opts := InstallOpts{ExecPath: "/usr/local/bin/mimis", ConfigPath: "/etc/mimisbaeti/config.yaml", User: "mimis"}
	if err := unitTemplate.Execute(&buf, opts); err != nil {
		t.Fatalf("executing template: %v", err)
	}

	out := buf.String()
	wantSubstrings := []string{
		"ExecStart=/usr/local/bin/mimis run --config /etc/mimisbaeti/config.yaml",
		"User=mimis",
		"WantedBy=multi-user.target",
	}
	for _, want := range wantSubstrings {
		if !strings.Contains(out, want) {
			t.Errorf("rendered unit missing %q\nfull output:\n%s", want, out)
		}
	}
}
