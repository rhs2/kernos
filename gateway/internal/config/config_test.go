package config

import (
	"bytes"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func mapLookup(m map[string]string) Lookup {
	return func(name string) (string, bool) {
		v, ok := m[name]
		return v, ok
	}
}

func TestSubstitution(t *testing.T) {
	env := map[string]string{"CRM_TOKEN": "s3cr3t-crm-token", "EMPTY": ""}
	secrets := NewSecrets()
	tree := map[string]any{
		"env":     map[string]any{"CRM_TOKEN": "Bearer ${CRM_TOKEN}"},
		"list":    []any{"${CRM_TOKEN}", "plain", 1.0, true, nil},
		"literal": "$${CRM_TOKEN}",
		"empty":   "${EMPTY}",
	}
	out, err := Substitute(tree, mapLookup(env), secrets)
	if err != nil {
		t.Fatal(err)
	}
	m := out.(map[string]any)
	if m["env"].(map[string]any)["CRM_TOKEN"] != "Bearer s3cr3t-crm-token" {
		t.Fatalf("substitution failed: %v", m["env"])
	}
	if m["list"].([]any)[0] != "s3cr3t-crm-token" || m["list"].([]any)[2] != 1.0 {
		t.Fatalf("list substitution failed: %v", m["list"])
	}
	if m["literal"] != "${CRM_TOKEN}" {
		t.Fatalf("$${VAR} must produce a literal, got %v", m["literal"])
	}
	if m["empty"] != "" || secrets.Count() != 1 {
		t.Fatalf("empty values are not secrets: %v %d", m["empty"], secrets.Count())
	}
	if _, err := Substitute("${MISSING_VAR}", mapLookup(env), secrets); err == nil || !strings.Contains(err.Error(), "MISSING_VAR") {
		t.Fatalf("unset variable must fail loudly, got %v", err)
	}
}

func TestStructuralKeysAreNotSecrets(t *testing.T) {
	env := map[string]string{
		"KERNOS_GATEWAY_DATA": "/var/lib/kernos-gateway",
		"HALCYON_LEDGER_DB":   "/var/lib/kernos-gateway/halcyon-ledger.db",
		"LEDGER_SCHEMA":       "/etc/kernos/ledger.sql",
		"MCP_BIN":             "/usr/local/bin/halcyon-crm-mcp",
		"CRM_TOKEN":           "crm-secret-value",
		"TINY":                "ab",
	}
	secrets := NewSecrets()
	tree := map[string]any{
		"data_dir": "${KERNOS_GATEWAY_DATA}",
		"connectors": []any{
			map[string]any{"name": "ledger", "type": "sqlite", "path": "${HALCYON_LEDGER_DB}", "init_sql": "${LEDGER_SCHEMA}"},
			map[string]any{"name": "crm", "type": "mcp", "command": []any{"${MCP_BIN}", "--flag"}, "env": map[string]any{"CRM_TOKEN": "${CRM_TOKEN}", "TINY": "${TINY}"}},
		},
	}
	out, err := Substitute(tree, mapLookup(env), secrets)
	if err != nil {
		t.Fatal(err)
	}
	m := out.(map[string]any)
	conns := m["connectors"].([]any)
	if m["data_dir"] != "/var/lib/kernos-gateway" || conns[0].(map[string]any)["path"] != "/var/lib/kernos-gateway/halcyon-ledger.db" {
		t.Fatalf("structural values not substituted: %v", m)
	}
	if conns[1].(map[string]any)["command"].([]any)[0] != "/usr/local/bin/halcyon-crm-mcp" {
		t.Fatalf("list under a structural key not substituted: %v", conns[1])
	}
	for _, structural := range []string{"/var/lib/kernos-gateway", "/var/lib/kernos-gateway/halcyon-ledger.db", "/etc/kernos/ledger.sql", "/usr/local/bin/halcyon-crm-mcp"} {
		if secrets.Contains(structural) {
			t.Fatalf("structural value %q must not be a secret", structural)
		}
	}
	if !secrets.Contains("crm-secret-value") {
		t.Fatal("credential under env must be a secret")
	}
	if secrets.Contains("ab") || secrets.Count() != 1 {
		t.Fatalf("values shorter than %d are not secrets: %d", MinSecretLength, secrets.Count())
	}
	if got := secrets.Redact("data at /var/lib/kernos-gateway with crm-secret-value"); got != "data at /var/lib/kernos-gateway with [redacted]" {
		t.Fatalf("Redact = %q", got)
	}
}

func TestNoLeak(t *testing.T) {
	secrets := NewSecrets()
	secrets.Add("s3cr3t-crm-token")
	secrets.Add("s3cr3t")
	if got := secrets.Redact("token=s3cr3t-crm-token and s3cr3t again"); got != "token=[redacted] and [redacted] again" {
		t.Fatalf("Redact = %q", got)
	}
	if !secrets.Contains("xx s3cr3t yy") || secrets.Contains("clean") {
		t.Fatal("Contains")
	}
	var buf bytes.Buffer
	log := slog.New(slog.NewJSONHandler(secrets.Writer(&buf), nil))
	log.Info("calling upstream", "authorization", "Bearer s3cr3t-crm-token", "run_id", "run_1")
	line := buf.String()
	if secrets.Contains(line) {
		t.Fatalf("secret leaked into the log: %s", line)
	}
	if !strings.Contains(line, Redacted) || !strings.Contains(line, "run_1") {
		t.Fatalf("log line lost content: %s", line)
	}
	if string(secrets.RedactBytes([]byte("no secrets here"))) != "no secrets here" {
		t.Fatal("RedactBytes must pass clean text through")
	}
}

func TestLoadFileAndEnv(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "gateway.json")
	body := `{
	  "listen": "127.0.0.1:17402",
	  "kernel_url": "http://127.0.0.1:17401",
	  "token": null,
	  "data_dir": "` + filepath.ToSlash(dir) + `",
	  "canary": {"interval_seconds": 2, "quarantine_after": 2, "auto_release": false},
	  "connectors": [
	    {"name": "crm", "type": "mcp", "command": ["fake-mcp"], "env": {"CRM_TOKEN": "${CRM_TOKEN}"}}
	  ]
	}`
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	env := map[string]string{
		"CRM_TOKEN":                      "crm-secret-value",
		"KERNOS_TOKEN":                   "kernel-shared-token",
		"KERNOS_CANARY_INTERVAL":         "5",
		"KERNOS_CANARY_AUTO_RELEASE":     "1",
		"KERNOS_GATEWAY_TEST_TOOLS":      "1",
		"KERNOS_GATEWAY_DATA":            filepath.Join(dir, "override"),
		"KERNOS_CANARY_QUARANTINE_AFTER": "3",
	}
	cfg, secrets, err := Load(path, mapLookup(env))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Listen != "127.0.0.1:17402" || cfg.KernelURL != "http://127.0.0.1:17401" {
		t.Fatalf("file values lost: %+v", cfg)
	}
	if cfg.Token != "kernel-shared-token" || cfg.DataDir != filepath.Join(dir, "override") {
		t.Fatalf("env overrides not applied: %+v", cfg)
	}
	if cfg.Canary.IntervalSeconds != 5 || !cfg.Canary.AutoRelease || cfg.Canary.QuarantineAfter != 3 || !cfg.TestTools {
		t.Fatalf("canary env overrides not applied: %+v", cfg.Canary)
	}
	if cfg.Connectors[0]["env"].(map[string]any)["CRM_TOKEN"] != "crm-secret-value" {
		t.Fatal("connector env not substituted")
	}
	if !secrets.Contains("crm-secret-value") || !secrets.Contains("kernel-shared-token") {
		t.Fatal("substituted values and the token must be secrets")
	}
	env["KERNOS_GATEWAY_KERNEL_URL"] = "http://127.0.0.1:1"
	env["KERNOS_KERNEL_URL"] = "http://127.0.0.1:2"
	cfg, _, err = Load(path, mapLookup(env))
	if err != nil || cfg.KernelURL != "http://127.0.0.1:1" {
		t.Fatalf("specific env name must win: %v %v", cfg.KernelURL, err)
	}
	delete(env, "CRM_TOKEN")
	if _, _, err := Load(path, mapLookup(env)); err == nil {
		t.Fatal("missing CRM_TOKEN must fail the load")
	}
	if _, _, err := Load(filepath.Join(dir, "missing.json"), mapLookup(env)); err == nil {
		t.Fatal("missing file must fail")
	}
	cfg, _, err = Load("", mapLookup(map[string]string{}))
	if err != nil || cfg.Listen != DefaultListen || cfg.Canary.IntervalSeconds != DefaultCanaryInterval {
		t.Fatalf("defaults: %+v %v", cfg, err)
	}
}

func TestValidate(t *testing.T) {
	cfg := Default()
	cfg.Connectors = []map[string]any{{"name": "a", "type": "sqlite"}, {"name": "a", "type": "http"}}
	if err := Validate(cfg); err == nil {
		t.Fatal("duplicate names must be rejected")
	}
	cfg = Default()
	cfg.Connectors = []map[string]any{{"type": "sqlite"}}
	if err := Validate(cfg); err == nil {
		t.Fatal("nameless connector must be rejected")
	}
	cfg = Default()
	cfg.Canary.QuarantineAfter = 0
	if err := Validate(cfg); err == nil {
		t.Fatal("zero quarantine_after must be rejected")
	}
	cfg = Default()
	cfg.LogFormat = "xml"
	if err := Validate(cfg); err == nil {
		t.Fatal("unknown log format must be rejected")
	}
	if err := ApplyEnv(Default(), mapLookup(map[string]string{"KERNOS_CANARY_INTERVAL": "soon"}), nil); err == nil {
		t.Fatal("bad interval must be rejected")
	}
}
