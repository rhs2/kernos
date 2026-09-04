package connect

import (
	"regexp"
	"sort"
	"strings"
	"time"
)

// Scope derivation names a connector may report in ToolSpec.ScopeDerivation.
const (
	// ScopeByTable derives `sql:table:<name>` scopes from a SQL statement.
	ScopeByTable = "table"
	// ScopeByHost derives `http:host:<hostname>` from a request URL.
	ScopeByHost = "host"
	// ScopeByPath derives `fs:path:<absolute directory>` from a file path.
	ScopeByPath = "path"
	// ScopeDeclared derives the scope from a template the upstream declared
	// (an MCP server's x-kernos-scope).
	ScopeDeclared = "declared"
	// ScopeNone means the connector cannot derive a scope; the remit must carry
	// the literal `<connector>:*`.
	ScopeNone = "none"
)

// TableScope returns the scope string for a SQL table.
func TableScope(table string) string { return "sql:table:" + table }

// HostScope returns the scope string for an HTTP host name.
func HostScope(host string) string { return "http:host:" + strings.ToLower(host) }

// PathScope returns the scope string for an absolute directory.
func PathScope(dir string) string { return "fs:path:" + dir }

// LiteralScope returns the scope a call requires when the connector cannot
// derive one: `<connector>:*`.
func LiteralScope(connector string) string { return connector + ":*" }

// MatchPattern reports whether value matches pattern. Matching is exact, or a
// prefix match when the pattern ends in `*`; it is case sensitive and knows
// no regular expressions, exactly as 03-REMIT describes for both tool and
// scope patterns.
func MatchPattern(pattern, value string) bool {
	if strings.HasSuffix(pattern, "*") {
		return strings.HasPrefix(value, pattern[:len(pattern)-1])
	}
	return pattern == value
}

// MatchAny reports whether any pattern matches the value.
func MatchAny(patterns []string, value string) bool {
	for _, p := range patterns {
		if MatchPattern(p, value) {
			return true
		}
	}
	return false
}

// UniqueSorted returns the distinct values in sorted order; scope lists are
// reported this way so responses and logs are stable.
func UniqueSorted(values []string) []string {
	seen := map[string]struct{}{}
	out := make([]string, 0, len(values))
	for _, v := range values {
		if _, ok := seen[v]; ok {
			continue
		}
		seen[v] = struct{}{}
		out = append(out, v)
	}
	sort.Strings(out)
	return out
}

var namePattern = regexp.MustCompile(`^[a-z][a-z0-9_]*$`)

// ValidName reports whether s is a valid connector or operation name:
// lowercase letters, digits and underscores, starting with a letter.
func ValidName(s string) bool { return namePattern.MatchString(s) }

// NormalizeName lowercases a name and replaces every character outside
// [a-z0-9_] with an underscore so upstream tool names become valid operation
// names. A name that starts with a digit gets a leading underscore replaced
// by "t_".
func NormalizeName(s string) string {
	var b strings.Builder
	for _, r := range strings.ToLower(s) {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '_':
			b.WriteRune(r)
		default:
			b.WriteByte('_')
		}
	}
	out := b.String()
	if out == "" || out[0] < 'a' || out[0] > 'z' {
		out = "t_" + out
	}
	return out
}

// ToolID joins a connector name and an operation into a tool identifier.
func ToolID(connector, operation string) string { return connector + "." + operation }

// Operation returns the operation part of a tool identifier for the given
// connector. It accepts both the full id `<connector>.<op>` and a bare
// operation, so a connector's Call works whichever form it is handed.
func Operation(connector, tool string) string {
	return strings.TrimPrefix(tool, connector+".")
}

// Timestamp formats a time the way every Kernos component does: RFC 3339 in
// UTC with millisecond precision.
func Timestamp(t time.Time) string {
	return t.UTC().Format("2006-01-02T15:04:05.000Z")
}
