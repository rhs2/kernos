// Package fsconn is the built-in "fs" connector: read, list and write under
// one configured root with the scope `fs:path:<absolute directory>` derived
// from the path argument. Paths that escape the root, through .. or through
// a symbolic link, are refused before anything is touched.
package fsconn

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"unicode/utf8"

	"github.com/rhs2/kernos/gateway/connect"
)

// TypeName is the value of "type" in gateway.json for this connector.
const TypeName = "fs"

func init() {
	connect.Register(New, TypeName)
}

// Connector is one root directory.
type Connector struct {
	name     string
	root     string
	tools    map[string]connect.ToolSpec
	order    []string
	maxBytes int64
	probe    connect.ProbeSpec
	hasProbe bool
}

var schemas = map[string]map[string]any{
	"read":  {"type": "object", "required": []any{"path"}, "properties": map[string]any{"path": map[string]any{"type": "string", "minLength": 1}}},
	"list":  {"type": "object", "required": []any{"path"}, "properties": map[string]any{"path": map[string]any{"type": "string", "minLength": 1}}},
	"write": {"type": "object", "required": []any{"path", "content"}, "properties": map[string]any{"path": map[string]any{"type": "string", "minLength": 1}, "content": map[string]any{"type": "string"}}},
}

var contracts = map[string]connect.Contract{
	"read":  {Required: map[string]string{"path": connect.TypeString, "content": connect.TypeString, "size": connect.TypeNumber}},
	"list":  {Required: map[string]string{"path": connect.TypeString, "entries": connect.TypeList}},
	"write": {Required: map[string]string{"path": connect.TypeString, "size": connect.TypeNumber, "written": connect.TypeBool}},
}

var descriptions = map[string]string{
	"read":  "Read a file under the root",
	"list":  "List a directory under the root",
	"write": "Write a file under the root",
}

// New is the Factory for the fs type. Configuration keys: root (required,
// made absolute), tools (read, list, write overrides; when present only the
// listed tools are exposed), max_bytes (default 4 MiB) and probe (default:
// list the root).
func New(cfg map[string]any) (connect.Connector, error) {
	name, err := connect.ConnectorName(cfg)
	if err != nil {
		return nil, err
	}
	root, err := connect.StringOf(cfg, "root")
	if err != nil || root == "" {
		return nil, fmt.Errorf("connector %s: root is required", name)
	}
	abs, err := filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("connector %s: root: %w", name, err)
	}
	if resolved, err := filepath.EvalSymlinks(abs); err == nil {
		abs = resolved
	}
	info, err := os.Stat(abs)
	if err != nil || !info.IsDir() {
		return nil, fmt.Errorf("connector %s: root %s is not a directory", name, abs)
	}
	maxBytes, err := connect.NumberOf(cfg, "max_bytes", 4<<20)
	if err != nil || maxBytes <= 0 {
		return nil, fmt.Errorf("connector %s: max_bytes must be a positive number", name)
	}
	c := &Connector{name: name, root: abs, tools: map[string]connect.ToolSpec{}, maxBytes: int64(maxBytes)}
	names, raws, err := connect.ToolsFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if len(names) == 0 {
		names = []string{"list", "read", "write"}
		raws = map[string]map[string]any{"read": {}, "list": {}, "write": {}}
	}
	for _, op := range names {
		if _, ok := schemas[op]; !ok {
			return nil, fmt.Errorf("connector %s: unknown tool %q (read, list and write exist)", name, op)
		}
		spec, err := connect.ToolFromConfig(name, op, raws[op], op == "write")
		if err != nil {
			return nil, fmt.Errorf("connector %s: %w", name, err)
		}
		if spec.Description == "" {
			spec.Description = descriptions[op]
		}
		if len(spec.InputSchema) == 0 {
			spec.InputSchema = schemas[op]
		}
		if len(spec.Contract.Required) == 0 {
			spec.Contract = contracts[op]
		}
		spec.ScopeDerivation = connect.ScopeByPath
		c.tools[op] = spec
		c.order = append(c.order, op)
	}
	probeTool, probeArgs, probeContract, hasProbe, err := connect.ProbeFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if hasProbe {
		spec, ok := c.tools[probeTool]
		if !ok {
			return nil, fmt.Errorf("connector %s: probe names unknown tool %q", name, probeTool)
		}
		c.probe = connect.ProbeSpec{Tool: probeTool, Args: probeArgs, Contract: spec.Contract}
		if probeContract != nil {
			c.probe.Contract = *probeContract
		}
		c.hasProbe = true
	} else if _, ok := c.tools["list"]; ok {
		c.probe = connect.ProbeSpec{Tool: "list", Args: map[string]any{"path": "."}, Contract: contracts["list"]}
		c.hasProbe = true
	}
	return c, nil
}

// Name implements connect.Connector.
func (c *Connector) Name() string { return c.name }

// Root returns the absolute root directory.
func (c *Connector) Root() string { return c.root }

// Tools implements connect.Connector.
func (c *Connector) Tools() []connect.ToolSpec {
	out := make([]connect.ToolSpec, 0, len(c.order))
	for _, op := range c.order {
		out = append(out, c.tools[op])
	}
	return out
}

// ProbeSpec implements connect.ProbeDescriber.
func (c *Connector) ProbeSpec() (connect.ProbeSpec, bool) { return c.probe, c.hasProbe }

// Probe implements connect.Connector.
func (c *Connector) Probe(ctx context.Context) (map[string]any, error) {
	if !c.hasProbe {
		return nil, connect.Deterministic("connector %s has no probe configured", c.name)
	}
	result, _, err := c.Call(ctx, c.probe.Tool, c.probe.Args)
	return result, err
}

// Resolve turns a path argument (relative to the root, or absolute inside
// it) into an absolute path and refuses anything outside the root, symbolic
// links included.
func (c *Connector) Resolve(raw string) (string, error) {
	if raw == "" {
		return "", connect.Deterministic("path is required")
	}
	p := raw
	if !filepath.IsAbs(p) {
		p = filepath.Join(c.root, p)
	}
	p = filepath.Clean(p)
	if !c.inside(p) {
		return "", connect.Deterministic("path %s escapes the root", raw)
	}
	// Resolve symbolic links on the longest existing prefix.
	existing := p
	for {
		if _, err := os.Lstat(existing); err == nil {
			break
		}
		parent := filepath.Dir(existing)
		if parent == existing {
			break
		}
		existing = parent
	}
	real, err := filepath.EvalSymlinks(existing)
	if err == nil && !c.inside(real) {
		return "", connect.Deterministic("path %s resolves outside the root", raw)
	}
	return p, nil
}

func (c *Connector) inside(p string) bool {
	return p == c.root || strings.HasPrefix(p, c.root+string(filepath.Separator))
}

// Scopes implements connect.ScopeDeriver: the directory of the file for read
// and write, the directory itself for list.
func (c *Connector) Scopes(toolName string, args map[string]any) ([]string, error) {
	op := connect.Operation(c.name, toolName)
	if _, ok := c.tools[op]; !ok {
		return nil, connect.Deterministic("unknown tool %s", toolName)
	}
	raw, _ := args["path"].(string)
	p, err := c.Resolve(raw)
	if err != nil {
		return nil, err
	}
	if op == "list" {
		return []string{connect.PathScope(p)}, nil
	}
	return []string{connect.PathScope(filepath.Dir(p))}, nil
}

// Call implements connect.Connector.
func (c *Connector) Call(ctx context.Context, toolName string, args map[string]any) (map[string]any, []string, error) {
	op := connect.Operation(c.name, toolName)
	scopes, err := c.Scopes(op, args)
	if err != nil {
		return nil, nil, err
	}
	raw, _ := args["path"].(string)
	p, err := c.Resolve(raw)
	if err != nil {
		return nil, scopes, err
	}
	if err := ctx.Err(); err != nil {
		return nil, scopes, err
	}
	switch op {
	case "read":
		info, err := os.Stat(p)
		if err != nil {
			return nil, scopes, classify(err)
		}
		if info.IsDir() {
			return nil, scopes, connect.Deterministic("%s is a directory", raw)
		}
		if info.Size() > c.maxBytes {
			return nil, scopes, connect.Deterministic("%s exceeds max_bytes (%d)", raw, c.maxBytes)
		}
		data, err := os.ReadFile(p)
		if err != nil {
			return nil, scopes, classify(err)
		}
		result := map[string]any{"path": p, "size": int64(len(data))}
		if utf8.Valid(data) {
			result["content"] = string(data)
			result["encoding"] = "utf-8"
		} else {
			result["content"] = base64.StdEncoding.EncodeToString(data)
			result["encoding"] = "base64"
		}
		return result, scopes, nil
	case "list":
		entries, err := os.ReadDir(p)
		if err != nil {
			return nil, scopes, classify(err)
		}
		list := make([]any, 0, len(entries))
		sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
		for _, e := range entries {
			item := map[string]any{"name": e.Name(), "dir": e.IsDir()}
			if info, err := e.Info(); err == nil {
				item["size"] = info.Size()
			}
			list = append(list, item)
		}
		return map[string]any{"path": p, "entries": list}, scopes, nil
	case "write":
		content, ok := args["content"].(string)
		if !ok {
			return nil, scopes, connect.Deterministic("content must be a string")
		}
		if int64(len(content)) > c.maxBytes {
			return nil, scopes, connect.Deterministic("content exceeds max_bytes (%d)", c.maxBytes)
		}
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			return nil, scopes, classify(err)
		}
		if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
			return nil, scopes, classify(err)
		}
		return map[string]any{"path": p, "size": int64(len(content)), "written": true}, scopes, nil
	}
	return nil, scopes, connect.Deterministic("unknown tool %s", toolName)
}

func classify(err error) error {
	if errors.Is(err, fs.ErrNotExist) || errors.Is(err, fs.ErrPermission) || errors.Is(err, fs.ErrExist) {
		return connect.Deterministic("%v", err)
	}
	var pe *os.PathError
	if errors.As(err, &pe) {
		msg := pe.Err.Error()
		if strings.Contains(msg, "not a directory") || strings.Contains(msg, "is a directory") {
			return connect.Deterministic("%v", err)
		}
	}
	return fmt.Errorf("filesystem error: %w", err)
}
