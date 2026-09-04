package fsconn

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/rhs2/kernos/gateway/connect"
)

func TestReadListWrite(t *testing.T) {
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "invoices"), 0o755)
	os.WriteFile(filepath.Join(root, "invoices", "inv-1001.txt"), []byte("Milk delivery"), 0o644)
	os.WriteFile(filepath.Join(root, "binary.bin"), []byte{0xff, 0xfe, 0x00}, 0o644)
	outside := t.TempDir()
	os.WriteFile(filepath.Join(outside, "secret.txt"), []byte("no"), 0o644)
	os.Symlink(outside, filepath.Join(root, "escape"))

	conn, err := New(map[string]any{"name": "docs", "type": "fs", "root": root})
	if err != nil {
		t.Fatal(err)
	}
	c := conn.(*Connector)
	realRoot := c.Root()
	tools := c.Tools()
	if len(tools) != 3 || tools[0].ID != "docs.list" || tools[2].ID != "docs.write" || !tools[2].Writes || tools[1].Writes {
		t.Fatalf("tools = %+v", tools)
	}
	if tools[0].ScopeDerivation != connect.ScopeByPath || tools[1].Contract.Required["content"] != "string" {
		t.Fatalf("spec = %+v", tools[1])
	}
	ctx := context.Background()

	scopes, err := c.Scopes("read", map[string]any{"path": "invoices/inv-1001.txt"})
	if err != nil || scopes[0] != "fs:path:"+filepath.Join(realRoot, "invoices") {
		t.Fatalf("read scope = %v %v", scopes, err)
	}
	scopes, err = c.Scopes("list", map[string]any{"path": "invoices"})
	if err != nil || scopes[0] != "fs:path:"+filepath.Join(realRoot, "invoices") {
		t.Fatalf("list scope = %v %v", scopes, err)
	}
	scopes, err = c.Scopes("docs.list", map[string]any{"path": "."})
	if err != nil || scopes[0] != "fs:path:"+realRoot {
		t.Fatalf("root scope = %v %v", scopes, err)
	}
	for _, bad := range []string{"../x", "invoices/../../x", filepath.Join(outside, "secret.txt"), "escape/secret.txt", ""} {
		if _, err := c.Scopes("read", map[string]any{"path": bad}); !connect.IsDeterministic(err) {
			t.Errorf("path %q must be refused deterministically, got %v", bad, err)
		}
	}

	result, _, err := c.Call(ctx, "read", map[string]any{"path": "invoices/inv-1001.txt"})
	if err != nil || result["content"] != "Milk delivery" || result["size"] != int64(13) || result["encoding"] != "utf-8" {
		t.Fatalf("read = %v %v", result, err)
	}
	if d := connect.CheckContract(tools[1].Contract, result); !d.OK() {
		t.Fatalf("read contract: %s", d)
	}
	result, _, err = c.Call(ctx, "read", map[string]any{"path": "binary.bin"})
	if err != nil || result["encoding"] != "base64" {
		t.Fatalf("binary read = %v %v", result, err)
	}
	if _, _, err := c.Call(ctx, "read", map[string]any{"path": "missing.txt"}); !connect.IsDeterministic(err) {
		t.Fatalf("missing file: %v", err)
	}
	if _, _, err := c.Call(ctx, "read", map[string]any{"path": "invoices"}); !connect.IsDeterministic(err) {
		t.Fatalf("reading a directory: %v", err)
	}

	result, _, err = c.Call(ctx, "list", map[string]any{"path": "."})
	if err != nil {
		t.Fatal(err)
	}
	entries := result["entries"].([]any)
	if len(entries) != 3 || entries[0].(map[string]any)["name"] != "binary.bin" || entries[2].(map[string]any)["dir"] != true {
		t.Fatalf("entries = %v", entries)
	}
	probe, err := c.Probe(ctx)
	if err != nil || connect.JSONType(probe["entries"]) != "list" {
		t.Fatalf("probe = %v %v", probe, err)
	}
	spec, ok := c.ProbeSpec()
	if !ok || spec.Tool != "list" {
		t.Fatalf("ProbeSpec = %+v", spec)
	}

	result, scopes, err = c.Call(ctx, "write", map[string]any{"path": "out/new/report.txt", "content": "hello"})
	if err != nil || result["written"] != true || result["size"] != int64(5) || scopes[0] != "fs:path:"+filepath.Join(realRoot, "out", "new") {
		t.Fatalf("write = %v %v %v", result, scopes, err)
	}
	if data, _ := os.ReadFile(filepath.Join(root, "out", "new", "report.txt")); string(data) != "hello" {
		t.Fatal("file not written")
	}
	if _, _, err := c.Call(ctx, "write", map[string]any{"path": "out/x.txt", "content": 5}); !connect.IsDeterministic(err) {
		t.Fatal("content must be a string")
	}
	if _, _, err := c.Call(ctx, "write", map[string]any{"path": "escape/pwn.txt", "content": "x"}); !connect.IsDeterministic(err) {
		t.Fatal("write through a symlink out of the root must be refused")
	}
	if _, err := os.Stat(filepath.Join(outside, "pwn.txt")); err == nil {
		t.Fatal("file was written outside the root")
	}
	if _, _, err := c.Call(ctx, "move", map[string]any{"path": "x"}); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool")
	}
	if _, err := c.Scopes("move", map[string]any{"path": "x"}); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool scope")
	}
	small, _ := New(map[string]any{"name": "docs", "root": root, "max_bytes": 4})
	if _, _, err := small.Call(ctx, "read", map[string]any{"path": "invoices/inv-1001.txt"}); !connect.IsDeterministic(err) || !strings.Contains(err.Error(), "max_bytes") {
		t.Fatalf("oversize read: %v", err)
	}
}

func TestConfig(t *testing.T) {
	if _, err := New(map[string]any{"name": "docs"}); err == nil {
		t.Fatal("root is required")
	}
	if _, err := New(map[string]any{"name": "docs", "root": filepath.Join(t.TempDir(), "missing")}); err == nil {
		t.Fatal("root must exist")
	}
	root := t.TempDir()
	if _, err := New(map[string]any{"name": "docs", "root": root, "tools": map[string]any{"delete": map[string]any{}}}); err == nil {
		t.Fatal("unknown tool")
	}
	c, err := New(map[string]any{"name": "docs", "root": root, "tools": map[string]any{"read": map[string]any{"description": "Read only"}}})
	if err != nil {
		t.Fatal(err)
	}
	if len(c.Tools()) != 1 || c.Tools()[0].Description != "Read only" {
		t.Fatalf("tools = %+v", c.Tools())
	}
	if _, ok := c.(*Connector).ProbeSpec(); ok {
		t.Fatal("no list tool means no default probe")
	}
	if _, err := c.Probe(context.Background()); !connect.IsDeterministic(err) {
		t.Fatal("probe without configuration")
	}
}
