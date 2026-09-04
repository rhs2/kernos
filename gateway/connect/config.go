package connect

import (
	"fmt"
	"sort"
)

// The helpers in this file read gateway.json values out of the untyped map a
// Factory receives. They exist so every connector reports configuration
// mistakes with the same wording.

// StringOf returns cfg[key] as a string. Missing or null is ("", nil); a
// value of another type is an error.
func StringOf(cfg map[string]any, key string) (string, error) {
	v, ok := cfg[key]
	if !ok || v == nil {
		return "", nil
	}
	s, ok := v.(string)
	if !ok {
		return "", fmt.Errorf("%s must be a string", key)
	}
	return s, nil
}

// BoolOf returns cfg[key] as a bool, or def when absent.
func BoolOf(cfg map[string]any, key string, def bool) (bool, error) {
	v, ok := cfg[key]
	if !ok || v == nil {
		return def, nil
	}
	b, ok := v.(bool)
	if !ok {
		return def, fmt.Errorf("%s must be a boolean", key)
	}
	return b, nil
}

// NumberOf returns cfg[key] as a float64, or def when absent.
func NumberOf(cfg map[string]any, key string, def float64) (float64, error) {
	v, ok := cfg[key]
	if !ok || v == nil {
		return def, nil
	}
	f, ok := number(v)
	if !ok {
		return def, fmt.Errorf("%s must be a number", key)
	}
	return f, nil
}

// MapOf returns cfg[key] as an object; missing or null is an empty map.
func MapOf(cfg map[string]any, key string) (map[string]any, error) {
	v, ok := cfg[key]
	if !ok || v == nil {
		return map[string]any{}, nil
	}
	m, ok := v.(map[string]any)
	if !ok {
		return nil, fmt.Errorf("%s must be an object", key)
	}
	return m, nil
}

// StringsOf returns cfg[key] as a list of strings; missing or null is empty.
func StringsOf(cfg map[string]any, key string) ([]string, error) {
	v, ok := cfg[key]
	if !ok || v == nil {
		return nil, nil
	}
	list, ok := v.([]any)
	if !ok {
		return nil, fmt.Errorf("%s must be a list of strings", key)
	}
	out := make([]string, 0, len(list))
	for i, item := range list {
		s, ok := item.(string)
		if !ok {
			return nil, fmt.Errorf("%s[%d] must be a string", key, i)
		}
		out = append(out, s)
	}
	return out, nil
}

// ConnectorName reads and validates the "name" of a connector entry.
func ConnectorName(cfg map[string]any) (string, error) {
	name, err := StringOf(cfg, "name")
	if err != nil {
		return "", err
	}
	if !ValidName(name) {
		return "", fmt.Errorf("connector name %q must match [a-z][a-z0-9_]*", name)
	}
	return name, nil
}

// ToolFromConfig reads the common tool fields (description, writes,
// input_schema, contract) of one entry under "tools" and returns a ToolSpec
// with ID set to `<connector>.<operation>`. Connector-specific keys such as
// "statement" are left to the caller. writesDefault is used when the entry
// does not say.
func ToolFromConfig(connector, operation string, raw map[string]any, writesDefault bool) (ToolSpec, error) {
	if !ValidName(operation) {
		return ToolSpec{}, fmt.Errorf("tool name %q must match [a-z][a-z0-9_]*", operation)
	}
	spec := ToolSpec{ID: ToolID(connector, operation)}
	var err error
	if spec.Description, err = StringOf(raw, "description"); err != nil {
		return spec, fmt.Errorf("tool %s: %w", operation, err)
	}
	if spec.Writes, err = BoolOf(raw, "writes", writesDefault); err != nil {
		return spec, fmt.Errorf("tool %s: %w", operation, err)
	}
	if spec.InputSchema, err = MapOf(raw, "input_schema"); err != nil {
		return spec, fmt.Errorf("tool %s: %w", operation, err)
	}
	if spec.Contract, err = ContractFromConfig(raw["contract"]); err != nil {
		return spec, fmt.Errorf("tool %s: %w", operation, err)
	}
	return spec, nil
}

// ToolsFromConfig reads the "tools" object of a connector entry in sorted
// order and returns the raw entry of each so the caller can pick up
// connector-specific keys.
func ToolsFromConfig(cfg map[string]any) ([]string, map[string]map[string]any, error) {
	tools, err := MapOf(cfg, "tools")
	if err != nil {
		return nil, nil, err
	}
	names := make([]string, 0, len(tools))
	raws := make(map[string]map[string]any, len(tools))
	for name, v := range tools {
		raw, ok := v.(map[string]any)
		if !ok {
			return nil, nil, fmt.Errorf("tools.%s must be an object", name)
		}
		names = append(names, name)
		raws[name] = raw
	}
	sort.Strings(names)
	return names, raws, nil
}

// ProbeFromConfig reads the "probe" object of a connector entry: the tool,
// its args and an optional contract override. When the entry names no probe
// the boolean is false.
func ProbeFromConfig(cfg map[string]any) (tool string, args map[string]any, contract *Contract, ok bool, err error) {
	raw, exists := cfg["probe"]
	if !exists || raw == nil {
		return "", nil, nil, false, nil
	}
	m, isMap := raw.(map[string]any)
	if !isMap {
		return "", nil, nil, false, fmt.Errorf("probe must be an object")
	}
	if tool, err = StringOf(m, "tool"); err != nil {
		return "", nil, nil, false, err
	}
	if tool == "" {
		return "", nil, nil, false, fmt.Errorf("probe.tool is required")
	}
	if args, err = MapOf(m, "args"); err != nil {
		return "", nil, nil, false, err
	}
	if c, exists := m["contract"]; exists && c != nil {
		parsed, err := ContractFromConfig(c)
		if err != nil {
			return "", nil, nil, false, fmt.Errorf("probe: %w", err)
		}
		contract = &parsed
	}
	return tool, args, contract, true, nil
}
