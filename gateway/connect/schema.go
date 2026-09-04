package connect

import (
	"fmt"
	"math"
	"regexp"
	"sort"
	"strings"
	"sync"
	"unicode/utf8"
)

// SchemaIssue is one violation found by ValidateSchema. Path is a dotted path
// from the root of the value ("" for the root itself).
type SchemaIssue struct {
	Path    string `json:"path"`
	Message string `json:"message"`
}

var (
	patternMu    sync.Mutex
	patternCache = map[string]*regexp.Regexp{}
)

func compilePattern(p string) (*regexp.Regexp, error) {
	patternMu.Lock()
	defer patternMu.Unlock()
	if re, ok := patternCache[p]; ok {
		return re, nil
	}
	re, err := regexp.Compile(p)
	if err != nil {
		return nil, err
	}
	patternCache[p] = re
	return re, nil
}

// ValidateSchema checks a JSON value against the JSON Schema subset the
// bundle format allows: type, required, properties, items, enum, minimum,
// maximum, minLength, maxLength, pattern and additionalProperties. Unknown
// keywords are ignored. A nil or empty schema accepts everything. The value
// must come from encoding/json (maps, slices, float64, string, bool, nil);
// integer types are also accepted as numbers.
func ValidateSchema(schema map[string]any, value any) []SchemaIssue {
	var issues []SchemaIssue
	validate(schema, value, "", &issues)
	return issues
}

func validate(schema map[string]any, value any, path string, issues *[]SchemaIssue) {
	if len(schema) == 0 {
		return
	}
	add := func(format string, args ...any) {
		*issues = append(*issues, SchemaIssue{Path: path, Message: fmt.Sprintf(format, args...)})
	}
	if t, ok := schema["type"]; ok {
		if !typeMatches(t, value) {
			add("expected type %s, got %s", describeType(t), jsonTypeName(value))
			return
		}
	}
	if enum, ok := schema["enum"].([]any); ok {
		found := false
		for _, e := range enum {
			if jsonEqual(e, value) {
				found = true
				break
			}
		}
		if !found {
			add("value is not one of the allowed values")
		}
	}
	switch v := value.(type) {
	case map[string]any:
		if req, ok := schema["required"].([]any); ok {
			for _, r := range req {
				name, _ := r.(string)
				if _, present := v[name]; !present {
					*issues = append(*issues, SchemaIssue{Path: join(path, name), Message: "required field is missing"})
				}
			}
		}
		props, _ := schema["properties"].(map[string]any)
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			if ps, ok := props[k].(map[string]any); ok {
				validate(ps, v[k], join(path, k), issues)
				continue
			}
			switch ap := schema["additionalProperties"].(type) {
			case bool:
				if !ap {
					*issues = append(*issues, SchemaIssue{Path: join(path, k), Message: "additional property is not allowed"})
				}
			case map[string]any:
				validate(ap, v[k], join(path, k), issues)
			}
		}
	case []any:
		if items, ok := schema["items"].(map[string]any); ok {
			for i, item := range v {
				validate(items, item, join(path, fmt.Sprint(i)), issues)
			}
		}
	case string:
		n := utf8.RuneCountInString(v)
		if min, ok := number(schema["minLength"]); ok && float64(n) < min {
			add("string is shorter than %d characters", int(min))
		}
		if max, ok := number(schema["maxLength"]); ok && float64(n) > max {
			add("string is longer than %d characters", int(max))
		}
		if p, ok := schema["pattern"].(string); ok {
			re, err := compilePattern(p)
			if err != nil {
				add("schema pattern is invalid: %v", err)
			} else if !re.MatchString(v) {
				add("string does not match pattern %q", p)
			}
		}
	default:
		if f, ok := number(value); ok {
			if min, ok := number(schema["minimum"]); ok && f < min {
				add("number is below the minimum %v", min)
			}
			if max, ok := number(schema["maximum"]); ok && f > max {
				add("number is above the maximum %v", max)
			}
		}
	}
}

func join(path, key string) string {
	if path == "" {
		return key
	}
	return path + "." + key
}

func number(v any) (float64, bool) {
	switch n := v.(type) {
	case float64:
		return n, true
	case float32:
		return float64(n), true
	case int:
		return float64(n), true
	case int64:
		return float64(n), true
	case int32:
		return float64(n), true
	case uint64:
		return float64(n), true
	case uint:
		return float64(n), true
	}
	return 0, false
}

func typeMatches(t any, value any) bool {
	switch tt := t.(type) {
	case string:
		return singleTypeMatches(tt, value)
	case []any:
		for _, one := range tt {
			if s, ok := one.(string); ok && singleTypeMatches(s, value) {
				return true
			}
		}
	}
	return false
}

func singleTypeMatches(t string, value any) bool {
	switch t {
	case "object":
		_, ok := value.(map[string]any)
		return ok
	case "array":
		_, ok := value.([]any)
		return ok
	case "string":
		_, ok := value.(string)
		return ok
	case "boolean":
		_, ok := value.(bool)
		return ok
	case "null":
		return value == nil
	case "number":
		_, ok := number(value)
		return ok
	case "integer":
		f, ok := number(value)
		return ok && f == math.Trunc(f)
	}
	return false
}

func describeType(t any) string {
	switch tt := t.(type) {
	case string:
		return tt
	case []any:
		parts := make([]string, 0, len(tt))
		for _, one := range tt {
			parts = append(parts, fmt.Sprint(one))
		}
		return strings.Join(parts, " or ")
	}
	return fmt.Sprint(t)
}

func jsonTypeName(v any) string {
	switch v.(type) {
	case nil:
		return "null"
	case map[string]any:
		return "object"
	case []any:
		return "array"
	case string:
		return "string"
	case bool:
		return "boolean"
	}
	if _, ok := number(v); ok {
		return "number"
	}
	return fmt.Sprintf("%T", v)
}

func jsonEqual(a, b any) bool {
	if fa, ok := number(a); ok {
		fb, ok := number(b)
		return ok && fa == fb
	}
	switch av := a.(type) {
	case nil:
		return b == nil
	case string:
		bv, ok := b.(string)
		return ok && av == bv
	case bool:
		bv, ok := b.(bool)
		return ok && av == bv
	case []any:
		bv, ok := b.([]any)
		if !ok || len(av) != len(bv) {
			return false
		}
		for i := range av {
			if !jsonEqual(av[i], bv[i]) {
				return false
			}
		}
		return true
	case map[string]any:
		bv, ok := b.(map[string]any)
		if !ok || len(av) != len(bv) {
			return false
		}
		for k, x := range av {
			y, ok := bv[k]
			if !ok || !jsonEqual(x, y) {
				return false
			}
		}
		return true
	}
	return false
}
