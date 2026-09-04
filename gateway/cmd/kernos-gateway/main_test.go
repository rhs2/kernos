package main

import (
	"bytes"
	"strings"
	"testing"
)

func TestVersionAndConfigErrors(t *testing.T) {
	var out, errOut bytes.Buffer
	if code := run([]string{"--version"}, &out, &errOut); code != 0 || !strings.Contains(out.String(), "kernos-gateway 0.1.0") {
		t.Fatalf("version: code %d out %q", code, out.String())
	}
	out.Reset()
	if code := run([]string{"--config", "/nonexistent/gateway.json"}, &out, &errOut); code != 2 || !strings.Contains(errOut.String(), "read config") {
		t.Fatalf("missing config: code %d err %q", code, errOut.String())
	}
	errOut.Reset()
	if code := run([]string{"--bogus"}, &out, &errOut); code != 2 {
		t.Fatalf("bad flag: code %d", code)
	}
}
