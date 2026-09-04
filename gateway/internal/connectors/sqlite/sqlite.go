// Package sqliteconn is the built-in "sqlite" connector: named statements
// with :param binding from the call's arguments plus :now, scope derived by
// parsing the table names after from, into, update and join, {"rows": [...]}
// for reads and the returning row for writes. It is the connector the
// reference bundle's ledger uses.
package sqliteconn

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"net/url"
	"os"
	"sort"
	"strings"
	"time"

	"modernc.org/sqlite"

	"github.com/rhs2/kernos/gateway/connect"
)

// TypeName is the value of "type" in gateway.json for this connector.
const TypeName = "sqlite"

func init() {
	connect.Register(New, TypeName)
}

type tool struct {
	spec      connect.ToolSpec
	statement string
	sqlText   string
	params    []string
	tables    []string
	read      bool
	returning bool
}

// Connector is one SQLite database with its named statements.
type Connector struct {
	name        string
	db          *sql.DB
	tools       map[string]*tool
	order       []string
	probe       connect.ProbeSpec
	hasProbe    bool
	now         func() time.Time
	initApplied bool
}

// New is the Factory for the sqlite type. Configuration keys: path
// (required), tools (name to {description, writes, statement, input_schema,
// contract}), probe ({tool, args, contract}) and init_sql: the path of a
// .sql file, or inline SQL, or a list of either, executed once when the
// database has no tables yet (freshly created or empty), which is how a
// container stack creates the schema itself. A database that already has a
// schema is never touched.
func New(cfg map[string]any) (connect.Connector, error) {
	name, err := connect.ConnectorName(cfg)
	if err != nil {
		return nil, err
	}
	path, err := connect.StringOf(cfg, "path")
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if path == "" {
		return nil, fmt.Errorf("connector %s: path is required", name)
	}
	c := &Connector{name: name, tools: map[string]*tool{}, now: time.Now}
	names, raws, err := connect.ToolsFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	for _, op := range names {
		raw := raws[op]
		statement, err := connect.StringOf(raw, "statement")
		if err != nil || strings.TrimSpace(statement) == "" {
			return nil, fmt.Errorf("connector %s: tool %s: statement is required", name, op)
		}
		t, err := compile(statement)
		if err != nil {
			return nil, fmt.Errorf("connector %s: tool %s: %w", name, op, err)
		}
		t.spec, err = connect.ToolFromConfig(name, op, raw, !t.read)
		if err != nil {
			return nil, fmt.Errorf("connector %s: %w", name, err)
		}
		t.spec.ScopeDerivation = connect.ScopeByTable
		if len(t.spec.InputSchema) == 0 {
			t.spec.InputSchema = derivedSchema(t.params)
		}
		c.tools[op] = t
		c.order = append(c.order, op)
	}
	probeTool, probeArgs, probeContract, hasProbe, err := connect.ProbeFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if hasProbe {
		t, ok := c.tools[probeTool]
		if !ok {
			return nil, fmt.Errorf("connector %s: probe names unknown tool %q", name, probeTool)
		}
		c.probe = connect.ProbeSpec{Tool: probeTool, Args: probeArgs, Contract: t.spec.Contract}
		if probeContract != nil {
			c.probe.Contract = *probeContract
		}
		c.hasProbe = true
	} else {
		c.probe = connect.ProbeSpec{Tool: "ping", Args: map[string]any{}, Contract: connect.Contract{Required: map[string]string{"rows": connect.TypeList}}}
		c.hasProbe = true
	}
	dsn := path
	if path != ":memory:" {
		dsn = "file:" + path + "?" + url.Values{"_pragma": []string{"busy_timeout(5000)", "journal_mode(WAL)", "foreign_keys(1)"}}.Encode()
	}
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("connector %s: open %s: %w", name, path, err)
	}
	if path == ":memory:" {
		db.SetMaxOpenConns(1)
	}
	c.db = db
	if err := c.runInit(cfg["init_sql"]); err != nil {
		db.Close()
		return nil, fmt.Errorf("connector %s: init_sql: %w", name, err)
	}
	return c, nil
}

// InitSQLApplied reports whether the last open ran init_sql, for tests and
// the startup log.
func (c *Connector) InitSQLApplied() bool { return c.initApplied }

func (c *Connector) runInit(raw any) error {
	var entries []string
	switch v := raw.(type) {
	case nil:
		return nil
	case string:
		entries = []string{v}
	case []any:
		for _, item := range v {
			s, ok := item.(string)
			if !ok {
				return fmt.Errorf("init_sql entries must be strings")
			}
			entries = append(entries, s)
		}
	default:
		return fmt.Errorf("init_sql must be a string or a list of strings")
	}
	var tables int
	if err := c.db.QueryRow(`select count(*) from sqlite_master where type in ('table', 'view')`).Scan(&tables); err != nil {
		return fmt.Errorf("inspecting the schema: %w", err)
	}
	if tables > 0 {
		return nil
	}
	for _, entry := range entries {
		entry = strings.TrimSpace(entry)
		if entry == "" {
			continue
		}
		script := entry
		if strings.HasSuffix(strings.ToLower(entry), ".sql") {
			data, err := os.ReadFile(entry)
			if err != nil {
				return fmt.Errorf("reading %s: %w", entry, err)
			}
			script = string(data)
		}
		if strings.TrimSpace(script) == "" {
			continue
		}
		if _, err := c.db.Exec(script); err != nil {
			return fmt.Errorf("executing %s: %w", describeInit(entry), err)
		}
	}
	c.initApplied = true
	return nil
}

func describeInit(entry string) string {
	if strings.HasSuffix(strings.ToLower(entry), ".sql") {
		return entry
	}
	return "inline init_sql"
}

func derivedSchema(params []string) map[string]any {
	req := make([]any, 0, len(params))
	props := map[string]any{}
	seen := map[string]bool{}
	for _, p := range params {
		if p == "now" || seen[p] {
			continue
		}
		seen[p] = true
		req = append(req, p)
		props[p] = map[string]any{}
	}
	return map[string]any{"type": "object", "required": req, "properties": props}
}

// SetClock replaces the clock behind :now, for tests.
func (c *Connector) SetClock(now func() time.Time) { c.now = now }

// Name implements connect.Connector.
func (c *Connector) Name() string { return c.name }

// Tools implements connect.Connector.
func (c *Connector) Tools() []connect.ToolSpec {
	out := make([]connect.ToolSpec, 0, len(c.order))
	for _, op := range c.order {
		out = append(out, c.tools[op].spec)
	}
	return out
}

// Scopes implements connect.ScopeDeriver: one sql:table scope per table the
// statement names, independent of the arguments.
func (c *Connector) Scopes(toolName string, _ map[string]any) ([]string, error) {
	t, ok := c.tools[connect.Operation(c.name, toolName)]
	if !ok {
		return nil, connect.Deterministic("unknown tool %s", toolName)
	}
	scopes := make([]string, 0, len(t.tables))
	for _, table := range t.tables {
		scopes = append(scopes, connect.TableScope(table))
	}
	return connect.UniqueSorted(scopes), nil
}

// ProbeSpec implements connect.ProbeDescriber.
func (c *Connector) ProbeSpec() (connect.ProbeSpec, bool) { return c.probe, c.hasProbe }

// Probe implements connect.Connector: the configured probe tool, or
// `select 1` when none is configured.
func (c *Connector) Probe(ctx context.Context) (map[string]any, error) {
	if c.probe.Tool == "ping" {
		if _, ok := c.tools["ping"]; !ok {
			rows, err := c.query(ctx, "select 1 as ok", nil)
			if err != nil {
				return nil, err
			}
			return map[string]any{"rows": rows}, nil
		}
	}
	result, _, err := c.Call(ctx, c.probe.Tool, c.probe.Args)
	return result, err
}

// Close releases the database.
func (c *Connector) Close() error { return c.db.Close() }

// Call implements connect.Connector.
func (c *Connector) Call(ctx context.Context, toolName string, args map[string]any) (map[string]any, []string, error) {
	op := connect.Operation(c.name, toolName)
	t, ok := c.tools[op]
	if !ok {
		return nil, nil, connect.Deterministic("unknown tool %s", toolName)
	}
	scopes, _ := c.Scopes(op, args)
	values := make([]any, 0, len(t.params))
	for _, p := range t.params {
		if p == "now" {
			values = append(values, connect.Timestamp(c.now()))
			continue
		}
		v, present := args[p]
		if !present {
			return nil, scopes, connect.Deterministic("argument %q required by the statement is missing", p)
		}
		bound, err := bindValue(v)
		if err != nil {
			return nil, scopes, connect.Deterministic("argument %q: %v", p, err)
		}
		values = append(values, bound)
	}
	if t.read {
		rows, err := c.query(ctx, t.sqlText, values)
		if err != nil {
			return nil, scopes, err
		}
		return map[string]any{"rows": rows}, scopes, nil
	}
	if t.returning {
		rows, err := c.query(ctx, t.sqlText, values)
		if err != nil {
			return nil, scopes, err
		}
		result := map[string]any{"rows_affected": int64(len(rows))}
		if len(rows) > 0 {
			for k, v := range rows[0] {
				result[k] = v
			}
		}
		if len(rows) > 1 {
			result["rows"] = rows
		}
		return result, scopes, nil
	}
	res, err := c.db.ExecContext(ctx, t.sqlText, values...)
	if err != nil {
		return nil, scopes, classify(err)
	}
	affected, _ := res.RowsAffected()
	last, _ := res.LastInsertId()
	return map[string]any{"rows_affected": affected, "last_insert_id": last}, scopes, nil
}

func (c *Connector) query(ctx context.Context, sqlText string, values []any) ([]map[string]any, error) {
	rows, err := c.db.QueryContext(ctx, sqlText, values...)
	if err != nil {
		return nil, classify(err)
	}
	defer rows.Close()
	cols, err := rows.Columns()
	if err != nil {
		return nil, classify(err)
	}
	out := []map[string]any{}
	for rows.Next() {
		raw := make([]any, len(cols))
		ptrs := make([]any, len(cols))
		for i := range raw {
			ptrs[i] = &raw[i]
		}
		if err := rows.Scan(ptrs...); err != nil {
			return nil, classify(err)
		}
		row := make(map[string]any, len(cols))
		for i, col := range cols {
			row[col] = fromSQL(raw[i])
		}
		out = append(out, row)
	}
	if err := rows.Err(); err != nil {
		return nil, classify(err)
	}
	return out, nil
}

func fromSQL(v any) any {
	switch x := v.(type) {
	case []byte:
		return string(x)
	case time.Time:
		return connect.Timestamp(x)
	}
	return v
}

func bindValue(v any) (any, error) {
	switch x := v.(type) {
	case nil, string, bool, int64:
		return x, nil
	case int:
		return int64(x), nil
	case float64:
		if x == math.Trunc(x) && math.Abs(x) < 1<<53 {
			return int64(x), nil
		}
		return x, nil
	case json.Number:
		if i, err := x.Int64(); err == nil {
			return i, nil
		}
		return x.Float64()
	case map[string]any, []any:
		b, err := json.Marshal(x)
		if err != nil {
			return nil, err
		}
		return string(b), nil
	}
	return nil, fmt.Errorf("unsupported value type %T", v)
}

// classify maps a database error to a deterministic failure (constraint,
// syntax, type errors) or an upstream error (busy, locked, I/O, cancelled).
func classify(err error) error {
	if err == nil {
		return nil
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	var se *sqlite.Error
	if errors.As(err, &se) {
		switch se.Code() & 0xff {
		case 5, 6, 7, 9, 10, 13, 14, 15, 26:
			// busy, locked, nomem, interrupt, ioerr, full, cantopen, protocol, notadb
			return fmt.Errorf("sqlite upstream error: %w", err)
		}
		return connect.Deterministic("%v", err)
	}
	if strings.Contains(err.Error(), "database is locked") || strings.Contains(err.Error(), "busy") {
		return fmt.Errorf("sqlite upstream error: %w", err)
	}
	return connect.Deterministic("%v", err)
}

// compile parses a statement once: the parameter list, the SQL with `?`
// placeholders, the tables it touches and whether it reads or returns rows.
func compile(statement string) (*tool, error) {
	toks := tokenize(statement)
	t := &tool{statement: statement}
	var b strings.Builder
	last := 0
	for _, tk := range toks {
		if tk.kind == kindParam {
			b.WriteString(statement[last:tk.start])
			b.WriteString("?")
			last = tk.end
			t.params = append(t.params, tk.text)
		}
	}
	b.WriteString(statement[last:])
	t.sqlText = b.String()
	t.tables = Tables(statement)
	first := ""
	for _, tk := range toks {
		if tk.kind == kindIdent {
			first = strings.ToLower(tk.text)
			break
		}
	}
	if first == "" {
		return nil, fmt.Errorf("statement is empty")
	}
	switch first {
	case "select", "with", "values", "pragma", "explain":
		t.read = true
	}
	for _, tk := range toks {
		if tk.kind == kindIdent && strings.EqualFold(tk.text, "returning") {
			t.returning = true
		}
	}
	if t.read && t.returning && first == "with" {
		// A CTE followed by a write with a returning clause is a write.
		t.read = false
	}
	return t, nil
}

// Params lists the :parameters of a statement in order, for tests and
// diagnostics.
func Params(statement string) []string {
	t, err := compile(statement)
	if err != nil {
		return nil
	}
	return t.params
}

var tableStopWords = map[string]bool{
	"where": true, "join": true, "left": true, "right": true, "inner": true, "outer": true, "cross": true,
	"natural": true, "on": true, "set": true, "values": true, "select": true, "group": true, "order": true,
	"limit": true, "returning": true, "using": true, "as": true, "full": true, "default": true, "and": true,
	"or": true, "not": true, "union": true, "except": true, "intersect": true, "having": true, "window": true,
}

// Tables returns the distinct table names a statement touches, in sorted
// order: the identifiers after from, into, update and join, including comma
// separated lists after from. Subqueries in parentheses are descended into
// because their own from clauses are scanned by the same pass.
func Tables(statement string) []string {
	toks := tokenize(statement)
	var tables []string
	for i := 0; i < len(toks); i++ {
		tk := toks[i]
		if tk.kind != kindIdent {
			continue
		}
		kw := strings.ToLower(tk.text)
		if kw != "from" && kw != "join" && kw != "into" && kw != "update" {
			continue
		}
		j := i + 1
		if kw == "update" && j < len(toks) && strings.EqualFold(toks[j].text, "or") {
			j += 2
		}
		for j < len(toks) {
			nt := toks[j]
			if nt.kind != kindIdent || tableStopWords[strings.ToLower(nt.text)] {
				break
			}
			tables = append(tables, stripSchema(nt.text))
			j++
			// optional alias: [as] ident
			if j < len(toks) && toks[j].kind == kindIdent && strings.EqualFold(toks[j].text, "as") {
				j++
			}
			if j < len(toks) && toks[j].kind == kindIdent && !tableStopWords[strings.ToLower(toks[j].text)] {
				j++
			}
			if kw == "from" && j < len(toks) && toks[j].kind == kindPunct && toks[j].text == "," {
				j++
				continue
			}
			break
		}
	}
	return connect.UniqueSorted(tables)
}

func stripSchema(name string) string {
	lower := strings.ToLower(name)
	for _, prefix := range []string{"main.", "temp."} {
		if strings.HasPrefix(lower, prefix) {
			return name[len(prefix):]
		}
	}
	return name
}

const (
	kindIdent = iota
	kindString
	kindParam
	kindPunct
)

type token struct {
	kind       int
	text       string
	start, end int
}

func isIdentStart(b byte) bool {
	return b == '_' || (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') || b >= 0x80
}

func isIdentChar(b byte) bool {
	return isIdentStart(b) || (b >= '0' && b <= '9') || b == '.' || b == '$'
}

// tokenize splits SQL into identifiers, string literals, :parameters and
// punctuation, skipping comments. Quoted identifiers ("x", `x`, [x]) become
// identifiers without their quotes.
func tokenize(s string) []token {
	var toks []token
	i := 0
	n := len(s)
	for i < n {
		b := s[i]
		switch {
		case b == ' ' || b == '\t' || b == '\n' || b == '\r':
			i++
		case b == '-' && i+1 < n && s[i+1] == '-':
			for i < n && s[i] != '\n' {
				i++
			}
		case b == '/' && i+1 < n && s[i+1] == '*':
			end := strings.Index(s[i+2:], "*/")
			if end < 0 {
				i = n
			} else {
				i += end + 4
			}
		case b == '\'':
			start := i
			i++
			for i < n {
				if s[i] == '\'' {
					if i+1 < n && s[i+1] == '\'' {
						i += 2
						continue
					}
					break
				}
				i++
			}
			i++
			if i > n {
				i = n
			}
			toks = append(toks, token{kind: kindString, text: s[start:i], start: start, end: i})
		case b == '"' || b == '`' || b == '[':
			closer := b
			if b == '[' {
				closer = ']'
			}
			start := i
			i++
			for i < n && s[i] != closer {
				i++
			}
			text := s[start+1 : i]
			if i < n {
				i++
			}
			toks = append(toks, token{kind: kindIdent, text: text, start: start, end: i})
		case b == ':' && i+1 < n && isIdentStart(s[i+1]):
			start := i
			i++
			for i < n && (isIdentChar(s[i]) && s[i] != '.') {
				i++
			}
			toks = append(toks, token{kind: kindParam, text: s[start+1 : i], start: start, end: i})
		case isIdentStart(b) || (b >= '0' && b <= '9'):
			start := i
			for i < n && isIdentChar(s[i]) {
				i++
			}
			toks = append(toks, token{kind: kindIdent, text: s[start:i], start: start, end: i})
		default:
			toks = append(toks, token{kind: kindPunct, text: string(b), start: i, end: i + 1})
			i++
		}
	}
	return toks
}

// SortedTables is Tables for callers that already hold a tool; exported for
// symmetry with Params.
func (c *Connector) SortedTables(op string) []string {
	t, ok := c.tools[op]
	if !ok {
		return nil
	}
	out := append([]string{}, t.tables...)
	sort.Strings(out)
	return out
}
