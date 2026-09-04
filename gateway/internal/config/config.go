// Package config loads gateway.json, applies the KERNOS_GATEWAY_* and
// KERNOS_* environment overrides, and substitutes ${VAR} references from the
// environment. Every substituted value is recorded as a secret so the rest of
// the gateway can keep it out of logs, responses and events.
package config

import (
	"encoding/json"
	"fmt"
	"os"
	"regexp"
	"strconv"
	"strings"
)

// Defaults of the gateway specification.
const (
	DefaultListen          = "127.0.0.1:7402"
	DefaultKernelURL       = "http://127.0.0.1:7401"
	DefaultDataDir         = "./gateway-data"
	DefaultCanaryInterval  = 60.0
	DefaultQuarantineAfter = 2
	DefaultCallTimeout     = 120
)

// Canary holds the canary loop settings.
type Canary struct {
	IntervalSeconds float64 `json:"interval_seconds"`
	QuarantineAfter int     `json:"quarantine_after"`
	AutoRelease     bool    `json:"auto_release"`
}

// Config is the gateway configuration after file, environment and
// substitution have been merged.
type Config struct {
	Listen             string           `json:"listen"`
	KernelURL          string           `json:"kernel_url"`
	Token              string           `json:"token"`
	DataDir            string           `json:"data_dir"`
	PublicKey          string           `json:"public_key"`
	LogFormat          string           `json:"log_format"`
	LogLevel           string           `json:"log_level"`
	CallTimeoutSeconds float64          `json:"call_timeout_seconds"`
	Canary             Canary           `json:"canary"`
	Connectors         []map[string]any `json:"connectors"`
	TestTools          bool             `json:"test_tools"`
}

// Default returns the configuration the gateway runs with when no file and
// no environment variable says otherwise.
func Default() *Config {
	return &Config{
		Listen:             DefaultListen,
		KernelURL:          DefaultKernelURL,
		DataDir:            DefaultDataDir,
		LogFormat:          "json",
		LogLevel:           "info",
		CallTimeoutSeconds: DefaultCallTimeout,
		Canary:             Canary{IntervalSeconds: DefaultCanaryInterval, QuarantineAfter: DefaultQuarantineAfter},
	}
}

// Lookup is the environment accessor Load uses; os.LookupEnv in production,
// a map in tests.
type Lookup func(name string) (string, bool)

// EnvLookup adapts os.LookupEnv.
func EnvLookup(name string) (string, bool) { return os.LookupEnv(name) }

// Load reads the file at path (optional: "" means defaults only), substitutes
// ${VAR} references, applies environment overrides and validates. The
// returned Secrets holds every value that came from the environment.
func Load(path string, lookup Lookup) (*Config, *Secrets, error) {
	secrets := NewSecrets()
	cfg := Default()
	if path != "" {
		raw, err := os.ReadFile(path)
		if err != nil {
			return nil, nil, fmt.Errorf("read config: %w", err)
		}
		if err := Parse(raw, cfg, lookup, secrets); err != nil {
			return nil, nil, err
		}
	}
	if err := ApplyEnv(cfg, lookup, secrets); err != nil {
		return nil, nil, err
	}
	if err := Validate(cfg); err != nil {
		return nil, nil, err
	}
	if cfg.Token != "" {
		secrets.Add(cfg.Token)
	}
	return cfg, secrets, nil
}

// Parse decodes gateway.json bytes into cfg after substituting ${VAR}
// references from lookup and recording the substituted values in secrets.
func Parse(raw []byte, cfg *Config, lookup Lookup, secrets *Secrets) error {
	var tree any
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	if err := dec.Decode(&tree); err != nil {
		return fmt.Errorf("parse config: %w", err)
	}
	substituted, err := Substitute(tree, lookup, secrets)
	if err != nil {
		return fmt.Errorf("config: %w", err)
	}
	clean, err := json.Marshal(substituted)
	if err != nil {
		return fmt.Errorf("config: %w", err)
	}
	if err := json.Unmarshal(clean, cfg); err != nil {
		return fmt.Errorf("parse config: %w", err)
	}
	return nil
}

var varPattern = regexp.MustCompile(`\$\$?\{([A-Za-z_][A-Za-z0-9_]*)\}`)

// StructuralKeys are configuration keys whose substituted values are
// addresses and paths rather than credentials: they are substituted but not
// registered as secrets, so a data directory or database path substituted
// from the environment still appears in logs and in the canary's
// repair_file field. Everything under any other key is treated as a secret.
var StructuralKeys = map[string]bool{
	"listen": true, "kernel_url": true, "data_dir": true, "public_key": true,
	"log_format": true, "log_level": true, "name": true, "type": true,
	"path": true, "root": true, "command": true, "allowed_hosts": true,
	"init_sql": true, "url": true,
}

// MinSecretLength is the shortest substituted value that is registered as a
// secret; redacting a one- or two-character value would corrupt every log
// line and response that happens to contain it.
const MinSecretLength = 4

// Substitute walks a decoded JSON tree and replaces every ${VAR} inside a
// string with the environment value. `$${VAR}` produces the literal text
// `${VAR}`. A reference to an unset variable is an error, because a missing
// credential must stop the gateway before it serves a call. Every
// substituted value outside StructuralKeys (and at least MinSecretLength
// long) is added to secrets.
func Substitute(v any, lookup Lookup, secrets *Secrets) (any, error) {
	return substitute(v, "", lookup, secrets)
}

func substitute(v any, key string, lookup Lookup, secrets *Secrets) (any, error) {
	switch t := v.(type) {
	case string:
		if StructuralKeys[key] {
			secrets = nil
		}
		return substituteString(t, lookup, secrets)
	case map[string]any:
		out := make(map[string]any, len(t))
		for k, x := range t {
			s, err := substitute(x, k, lookup, secrets)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", k, err)
			}
			out[k] = s
		}
		return out, nil
	case []any:
		out := make([]any, len(t))
		for i, x := range t {
			s, err := substitute(x, key, lookup, secrets)
			if err != nil {
				return nil, fmt.Errorf("[%d]: %w", i, err)
			}
			out[i] = s
		}
		return out, nil
	}
	return v, nil
}

func substituteString(s string, lookup Lookup, secrets *Secrets) (string, error) {
	var firstErr error
	out := varPattern.ReplaceAllStringFunc(s, func(m string) string {
		if strings.HasPrefix(m, "$$") {
			return m[1:]
		}
		name := m[2 : len(m)-1]
		val, ok := lookup(name)
		if !ok {
			if firstErr == nil {
				firstErr = fmt.Errorf("environment variable %s referenced by the configuration is not set", name)
			}
			return ""
		}
		if secrets != nil && len(val) >= MinSecretLength {
			secrets.Add(val)
		}
		return val
	})
	return out, firstErr
}

// ApplyEnv overrides fields from the environment. The specific
// KERNOS_GATEWAY_* name wins over the shared KERNOS_* name when both are set.
func ApplyEnv(cfg *Config, lookup Lookup, secrets *Secrets) error {
	first := func(names ...string) (string, bool) {
		for _, n := range names {
			if v, ok := lookup(n); ok && v != "" {
				return v, true
			}
		}
		return "", false
	}
	if v, ok := first("KERNOS_GATEWAY_LISTEN"); ok {
		cfg.Listen = v
	}
	if v, ok := first("KERNOS_GATEWAY_KERNEL_URL", "KERNOS_KERNEL_URL"); ok {
		cfg.KernelURL = v
	}
	if v, ok := first("KERNOS_GATEWAY_TOKEN", "KERNOS_TOKEN"); ok {
		cfg.Token = v
		if secrets != nil {
			secrets.Add(v)
		}
	}
	if v, ok := first("KERNOS_GATEWAY_DATA"); ok {
		cfg.DataDir = v
	}
	if v, ok := first("KERNOS_GATEWAY_PUBLIC_KEY", "KERNOS_PUBLIC_KEY"); ok {
		cfg.PublicKey = v
	}
	if v, ok := first("KERNOS_GATEWAY_LOG"); ok {
		cfg.LogFormat = v
	}
	if v, ok := first("KERNOS_GATEWAY_LOG_LEVEL"); ok {
		cfg.LogLevel = v
	}
	if v, ok := first("KERNOS_GATEWAY_CALL_TIMEOUT"); ok {
		f, err := strconv.ParseFloat(v, 64)
		if err != nil || f <= 0 {
			return fmt.Errorf("KERNOS_GATEWAY_CALL_TIMEOUT must be a positive number of seconds")
		}
		cfg.CallTimeoutSeconds = f
	}
	if v, ok := first("KERNOS_GATEWAY_CANARY_INTERVAL", "KERNOS_CANARY_INTERVAL"); ok {
		f, err := strconv.ParseFloat(v, 64)
		if err != nil || f <= 0 {
			return fmt.Errorf("KERNOS_CANARY_INTERVAL must be a positive number of seconds")
		}
		cfg.Canary.IntervalSeconds = f
	}
	if v, ok := first("KERNOS_GATEWAY_CANARY_QUARANTINE_AFTER", "KERNOS_CANARY_QUARANTINE_AFTER"); ok {
		n, err := strconv.Atoi(v)
		if err != nil || n <= 0 {
			return fmt.Errorf("KERNOS_CANARY_QUARANTINE_AFTER must be a positive integer")
		}
		cfg.Canary.QuarantineAfter = n
	}
	if v, ok := first("KERNOS_GATEWAY_CANARY_AUTO_RELEASE", "KERNOS_CANARY_AUTO_RELEASE"); ok {
		cfg.Canary.AutoRelease = truthy(v)
	}
	if v, ok := first("KERNOS_GATEWAY_TEST_TOOLS"); ok {
		cfg.TestTools = truthy(v)
	}
	return nil
}

func truthy(v string) bool {
	switch strings.ToLower(strings.TrimSpace(v)) {
	case "1", "true", "yes", "on":
		return true
	}
	return false
}

// Validate checks the merged configuration for values the gateway cannot
// run with.
func Validate(cfg *Config) error {
	if cfg.Listen == "" {
		return fmt.Errorf("listen must not be empty")
	}
	if cfg.KernelURL == "" && cfg.PublicKey == "" {
		return fmt.Errorf("kernel_url or public_key is required to verify remits")
	}
	if cfg.DataDir == "" {
		return fmt.Errorf("data_dir must not be empty")
	}
	if cfg.Canary.IntervalSeconds <= 0 {
		return fmt.Errorf("canary.interval_seconds must be positive")
	}
	if cfg.Canary.QuarantineAfter <= 0 {
		return fmt.Errorf("canary.quarantine_after must be positive")
	}
	if cfg.CallTimeoutSeconds <= 0 {
		return fmt.Errorf("call_timeout_seconds must be positive")
	}
	switch cfg.LogFormat {
	case "", "json", "text":
	default:
		return fmt.Errorf("log_format must be json or text")
	}
	seen := map[string]bool{}
	for i, c := range cfg.Connectors {
		name, _ := c["name"].(string)
		typ, _ := c["type"].(string)
		if name == "" || typ == "" {
			return fmt.Errorf("connectors[%d]: name and type are required", i)
		}
		if seen[name] {
			return fmt.Errorf("connectors[%d]: duplicate connector name %q", i, name)
		}
		seen[name] = true
	}
	return nil
}
