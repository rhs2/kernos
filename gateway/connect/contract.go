package connect

import (
	"encoding/json"
	"fmt"
	"sort"
)

// JSON type names a contract may use.
const (
	TypeString = "string"
	TypeNumber = "number"
	TypeBool   = "bool"
	TypeObject = "object"
	TypeList   = "list"
)

// JSONType names the contract type of a decoded JSON value: string, number,
// bool, object, list, or null. Values that did not come from JSON are
// reported by their Go type so a mismatch is still explained.
func JSONType(v any) string {
	switch v.(type) {
	case nil:
		return "null"
	case string:
		return TypeString
	case bool:
		return TypeBool
	case map[string]any:
		return TypeObject
	case []any, []map[string]any, []string:
		return TypeList
	case json.Number:
		return TypeNumber
	}
	if _, ok := number(v); ok {
		return TypeNumber
	}
	return fmt.Sprintf("%T", v)
}

// Shape describes the structure of a value: objects become a map from field
// to shape, everything else becomes its JSON type name. Repair requests
// record the observed shape of a failing probe with it.
func Shape(v any) any {
	if m, ok := v.(map[string]any); ok {
		out := make(map[string]any, len(m))
		for k, x := range m {
			out[k] = Shape(x)
		}
		return out
	}
	return JSONType(v)
}

// TypeMismatch records a field that was present with the wrong JSON type.
type TypeMismatch struct {
	Expected string `json:"expected"`
	Observed string `json:"observed"`
}

// Diff is the result of a contract check. Missing lists required fields the
// response lacked, TypeMismatch the fields with the wrong type and
// UnexpectedRequired the input parameters the upstream now requires that the
// gateway did not know about (written as `<operation>.<parameter>`).
type Diff struct {
	Missing            []string                `json:"missing"`
	TypeMismatch       map[string]TypeMismatch `json:"type_mismatch"`
	UnexpectedRequired []string                `json:"unexpected_required"`
}

// OK reports whether the diff is empty, that is, the contract holds.
func (d Diff) OK() bool {
	return len(d.Missing) == 0 && len(d.TypeMismatch) == 0 && len(d.UnexpectedRequired) == 0
}

// Normalized returns a copy with no nil collections so it marshals as
// `{"missing": [], "type_mismatch": {}, "unexpected_required": []}`.
func (d Diff) Normalized() Diff {
	out := Diff{
		Missing:            append([]string{}, d.Missing...),
		TypeMismatch:       map[string]TypeMismatch{},
		UnexpectedRequired: append([]string{}, d.UnexpectedRequired...),
	}
	for k, v := range d.TypeMismatch {
		out.TypeMismatch[k] = v
	}
	sort.Strings(out.Missing)
	sort.Strings(out.UnexpectedRequired)
	return out
}

// MarshalJSON always emits the normalized form.
func (d Diff) MarshalJSON() ([]byte, error) {
	type plain Diff
	return json.Marshal(plain(d.Normalized()))
}

// String renders the diff in one line for logs and error details.
func (d Diff) String() string {
	n := d.Normalized()
	keys := make([]string, 0, len(n.TypeMismatch))
	for k := range n.TypeMismatch {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	mm := make([]string, 0, len(keys))
	for _, k := range keys {
		mm = append(mm, fmt.Sprintf("%s expected %s observed %s", k, n.TypeMismatch[k].Expected, n.TypeMismatch[k].Observed))
	}
	return fmt.Sprintf("missing=%v type_mismatch=%v unexpected_required=%v", n.Missing, mm, n.UnexpectedRequired)
}

// CheckContract compares a response against a contract: every required field
// must be present with the declared JSON type; extra fields are ignored.
func CheckContract(c Contract, result map[string]any) Diff {
	var d Diff
	names := make([]string, 0, len(c.Required))
	for name := range c.Required {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		want := c.Required[name]
		v, ok := result[name]
		if !ok {
			d.Missing = append(d.Missing, name)
			continue
		}
		got := JSONType(v)
		if got != want {
			if d.TypeMismatch == nil {
				d.TypeMismatch = map[string]TypeMismatch{}
			}
			d.TypeMismatch[name] = TypeMismatch{Expected: want, Observed: got}
		}
	}
	return d
}

// NormalizeTypeName maps the synonyms configuration files tend to use
// ("boolean", "array", "integer") onto the five contract type names. It
// returns false for a name it does not know.
func NormalizeTypeName(t string) (string, bool) {
	switch t {
	case TypeString, TypeNumber, TypeBool, TypeObject, TypeList:
		return t, true
	case "boolean":
		return TypeBool, true
	case "array":
		return TypeList, true
	case "integer", "float", "int":
		return TypeNumber, true
	}
	return "", false
}

// ContractFromConfig parses `{"required": {"field": "type"}}` from a
// configuration value. A nil value is an empty contract.
func ContractFromConfig(raw any) (Contract, error) {
	c := Contract{Required: map[string]string{}}
	if raw == nil {
		return c, nil
	}
	m, ok := raw.(map[string]any)
	if !ok {
		return c, fmt.Errorf("contract must be an object")
	}
	req, ok := m["required"]
	if !ok || req == nil {
		return c, nil
	}
	fields, ok := req.(map[string]any)
	if !ok {
		return c, fmt.Errorf("contract.required must be an object of field to type")
	}
	for name, t := range fields {
		ts, ok := t.(string)
		if !ok {
			return c, fmt.Errorf("contract.required.%s must be a type name", name)
		}
		norm, ok := NormalizeTypeName(ts)
		if !ok {
			return c, fmt.Errorf("contract.required.%s: unknown type %q", name, ts)
		}
		c.Required[name] = norm
	}
	return c, nil
}

// RequiredInputs lists the required input parameters of a tool as
// `<operation>.<parameter>` strings; the canary loop compares two snapshots
// of them to detect new required parameters upstream.
func RequiredInputs(connector string, tools []ToolSpec) []string {
	var out []string
	for _, t := range tools {
		req, _ := t.InputSchema["required"].([]any)
		for _, r := range req {
			if s, ok := r.(string); ok {
				out = append(out, Operation(connector, t.ID)+"."+s)
			}
		}
	}
	sort.Strings(out)
	return out
}
