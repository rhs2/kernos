package sqliteconn

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

const schema = `
create table if not exists vendors(id integer primary key, name text not null, terms text);
create table if not exists ledger_entries(id integer primary key, invoice_id text not null unique, vendor text not null, account text not null, amount real not null, posted_at text not null, voided_at text, void_reason text);
insert into vendors(name, terms) values ('Northwind Dairy', 'net30');
`

func ledgerConfig(t *testing.T, path string) map[string]any {
	t.Helper()
	var cfg map[string]any
	raw := `{"name": "ledger", "type": "sqlite", "path": "` + filepath.ToSlash(path) + `",
	  "init_sql": ` + mustJSON(schema) + `,
	  "tools": {
	    "post_entry": {"description": "Post a journal entry", "writes": true,
	      "statement": "insert into ledger_entries(invoice_id, vendor, account, amount, posted_at) values (:invoice_id, :vendor, :account, :amount, :now) returning id as entry_id, posted_at",
	      "input_schema": {"type": "object", "required": ["invoice_id", "vendor", "account", "amount"]},
	      "contract": {"required": {"entry_id": "number", "posted_at": "string"}}},
	    "void_entry": {"description": "Void a posted entry", "writes": true,
	      "statement": "update ledger_entries set voided_at=:now, void_reason=:reason where id=:entry_id returning id as entry_id, voided_at",
	      "contract": {"required": {"entry_id": "number", "voided_at": "string"}}},
	    "lookup_vendor": {"description": "Find a vendor", "writes": false,
	      "statement": "select id, name, terms from vendors where name = :name",
	      "contract": {"required": {"rows": "list"}}},
	    "count_entries": {"statement": "select count(*) as n from ledger_entries"},
	    "delete_entry": {"statement": "delete from ledger_entries where id = :entry_id"}
	  },
	  "probe": {"tool": "lookup_vendor", "args": {"name": "__probe__"}}}`
	if err := json.Unmarshal([]byte(raw), &cfg); err != nil {
		t.Fatal(err)
	}
	return cfg
}

func mustJSON(s string) string {
	b, _ := json.Marshal(s)
	return string(b)
}

func TestTablesAndParams(t *testing.T) {
	cases := map[string][]string{
		"select id, name, terms from vendors where name = :name":                                                   {"vendors"},
		"insert into ledger_entries(invoice_id, vendor) values (:invoice_id, :vendor) returning id as entry_id":    {"ledger_entries"},
		"update ledger_entries set voided_at=:now where id=:entry_id returning id":                                 {"ledger_entries"},
		"update or replace ledger_entries set x=1":                                                                 {"ledger_entries"},
		"delete from ledger_entries where id = :id":                                                                {"ledger_entries"},
		"select * from invoices i join vendors v on v.id = i.vendor_id left join accounts a on a.id = i.account":   {"accounts", "invoices", "vendors"},
		"select * from invoices, vendors as v where v.id = invoices.vendor_id":                                     {"invoices", "vendors"},
		"select * from (select * from ledger_entries) sub where sub.amount > :min":                                 {"ledger_entries"},
		"insert into ledger_entries select * from staging_entries -- from comments\n where 'from quoted' = :x":     {"ledger_entries", "staging_entries"},
		"select * from \"Quoted Table\" q, `other` o, [bracket] b":                                                 {"Quoted Table", "bracket", "other"},
		"select * from main.vendors":                                                                               {"vendors"},
		"with recent as (select * from ledger_entries where posted_at > :since) select * from recent join vendors": {"ledger_entries", "recent", "vendors"},
		"select 1": {},
	}
	for stmt, want := range cases {
		got := Tables(stmt)
		if len(got) != len(want) {
			t.Errorf("Tables(%q) = %v, want %v", stmt, got, want)
			continue
		}
		for i := range want {
			if got[i] != want[i] {
				t.Errorf("Tables(%q) = %v, want %v", stmt, got, want)
			}
		}
	}
	params := Params("insert into t(a, b, c) values (:a, :b, :now) /* :ignored */ -- :also\n where x = ':literal' and y = :a")
	if len(params) != 4 || params[0] != "a" || params[1] != "b" || params[2] != "now" || params[3] != "a" {
		t.Fatalf("Params = %v", params)
	}
	tl, err := compile("select * from t where x = :a")
	if err != nil || tl.sqlText != "select * from t where x = ?" || !tl.read || tl.returning {
		t.Fatalf("compile = %+v %v", tl, err)
	}
	if _, err := compile("   "); err == nil {
		t.Fatal("empty statement must fail")
	}
}

func TestLedgerConnector(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "ledger.db")
	conn, err := New(ledgerConfig(t, path))
	if err != nil {
		t.Fatal(err)
	}
	c := conn.(*Connector)
	defer c.Close()
	fixed := time.Date(2026, 9, 4, 12, 0, 0, 123_000_000, time.UTC)
	c.SetClock(func() time.Time { return fixed })

	tools := c.Tools()
	if len(tools) != 5 || tools[0].ID != "ledger.count_entries" || tools[3].ID != "ledger.post_entry" {
		t.Fatalf("tools = %+v", tools)
	}
	byID := map[string]connect.ToolSpec{}
	for _, tl := range tools {
		byID[tl.ID] = tl
	}
	if !byID["ledger.post_entry"].Writes || byID["ledger.lookup_vendor"].Writes || !byID["ledger.delete_entry"].Writes || byID["ledger.count_entries"].Writes {
		t.Fatal("writes must come from config or be inferred from the statement")
	}
	if byID["ledger.void_entry"].InputSchema["required"].([]any)[0] != "reason" || len(byID["ledger.void_entry"].InputSchema["required"].([]any)) != 2 {
		t.Fatalf("derived input schema: %v", byID["ledger.void_entry"].InputSchema)
	}
	if byID["ledger.post_entry"].ScopeDerivation != connect.ScopeByTable || byID["ledger.post_entry"].Contract.Required["entry_id"] != "number" {
		t.Fatalf("spec = %+v", byID["ledger.post_entry"])
	}

	scopes, err := c.Scopes("post_entry", nil)
	if err != nil || len(scopes) != 1 || scopes[0] != "sql:table:ledger_entries" {
		t.Fatalf("Scopes = %v %v", scopes, err)
	}
	if scopes, _ := c.Scopes("ledger.lookup_vendor", nil); scopes[0] != "sql:table:vendors" {
		t.Fatal("Scopes must accept the full tool id")
	}
	if _, err := c.Scopes("nope", nil); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool must be deterministic")
	}

	result, scopes, err := c.Call(ctx, "post_entry", map[string]any{"invoice_id": "inv-1001", "vendor": "Northwind Dairy", "account": "5100", "amount": 1234.56})
	if err != nil {
		t.Fatal(err)
	}
	if result["entry_id"] != int64(1) || result["posted_at"] != "2026-09-04T12:00:00.123Z" || result["rows_affected"] != int64(1) || scopes[0] != "sql:table:ledger_entries" {
		t.Fatalf("post_entry = %v %v", result, scopes)
	}
	if d := connect.CheckContract(byID["ledger.post_entry"].Contract, result); !d.OK() {
		t.Fatalf("post_entry contract: %s", d)
	}
	_, _, err = c.Call(ctx, "post_entry", map[string]any{"invoice_id": "inv-1001", "vendor": "x", "account": "y", "amount": 1.0})
	if !connect.IsDeterministic(err) {
		t.Fatalf("unique violation must be deterministic, got %v", err)
	}
	_, _, err = c.Call(ctx, "post_entry", map[string]any{"invoice_id": "inv-1002"})
	if !connect.IsDeterministic(err) {
		t.Fatalf("missing argument must be deterministic, got %v", err)
	}

	result, _, err = c.Call(ctx, "lookup_vendor", map[string]any{"name": "Northwind Dairy"})
	if err != nil {
		t.Fatal(err)
	}
	rows := result["rows"].([]map[string]any)
	if len(rows) != 1 || rows[0]["name"] != "Northwind Dairy" || rows[0]["id"] != int64(1) || rows[0]["terms"] != "net30" {
		t.Fatalf("lookup rows = %v", rows)
	}
	result, _, err = c.Call(ctx, "lookup_vendor", map[string]any{"name": "__probe__"})
	if err != nil || len(result["rows"].([]map[string]any)) != 0 {
		t.Fatalf("empty select must return an empty list: %v %v", result, err)
	}
	if d := connect.CheckContract(byID["ledger.lookup_vendor"].Contract, result); !d.OK() {
		t.Fatalf("empty rows must satisfy list: %s", d)
	}
	probe, err := c.Probe(ctx)
	if err != nil || connect.JSONType(probe["rows"]) != "list" {
		t.Fatalf("probe = %v %v", probe, err)
	}
	spec, ok := c.ProbeSpec()
	if !ok || spec.Tool != "lookup_vendor" || spec.Contract.Required["rows"] != "list" {
		t.Fatalf("ProbeSpec = %+v %v", spec, ok)
	}

	result, _, err = c.Call(ctx, "void_entry", map[string]any{"entry_id": float64(1), "reason": "run abandoned"})
	if err != nil || result["entry_id"] != int64(1) || result["voided_at"] != "2026-09-04T12:00:00.123Z" {
		t.Fatalf("void_entry = %v %v", result, err)
	}
	result, _, err = c.Call(ctx, "void_entry", map[string]any{"entry_id": 99, "reason": "none"})
	if err != nil || result["rows_affected"] != int64(0) || result["entry_id"] != nil {
		t.Fatalf("void of a missing row: %v %v", result, err)
	}
	result, _, err = c.Call(ctx, "count_entries", map[string]any{})
	if err != nil || result["rows"].([]map[string]any)[0]["n"] != int64(1) {
		t.Fatalf("count = %v %v", result, err)
	}
	result, _, err = c.Call(ctx, "delete_entry", map[string]any{"entry_id": 1})
	if err != nil || result["rows_affected"] != int64(1) {
		t.Fatalf("delete = %v %v", result, err)
	}
	if _, _, err := c.Call(ctx, "missing_tool", nil); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool")
	}
}

const halcyonLedgerSQL = `-- Halcyon Provisions ledger (fictional)
create table vendors (
  id    integer primary key,
  name  text not null,
  terms text
);

create table ledger_entries (
  id          integer primary key,
  invoice_id  text not null,
  vendor      text,
  account     text,
  amount      real,
  posted_at   text,
  voided_at   text,
  void_reason text
);

create index ledger_entries_invoice_id on ledger_entries (invoice_id);

insert into vendors (name, terms) values
  ('Northwind Dairy', 'net 30'),
  ('Harbor Greens', 'net 14'),
  ('Millstone Bakery', 'net 30');
`

func TestInitSQLFileRunsOnceOnAnEmptyDatabase(t *testing.T) {
	dir := t.TempDir()
	schema := filepath.Join(dir, "ledger.sql")
	if err := os.WriteFile(schema, []byte(halcyonLedgerSQL), 0o644); err != nil {
		t.Fatal(err)
	}
	dbPath := filepath.Join(dir, "halcyon-ledger.db")
	cfg := func() map[string]any {
		return map[string]any{"name": "ledger", "type": "sqlite", "path": dbPath, "init_sql": schema,
			"tools": map[string]any{"lookup_vendor": map[string]any{"statement": "select id, name, terms from vendors where name = :name"}}}
	}
	// First open: the database file does not exist, so the schema is created.
	conn, err := New(cfg())
	if err != nil {
		t.Fatal(err)
	}
	c := conn.(*Connector)
	if !c.InitSQLApplied() {
		t.Fatal("init_sql must run on a fresh database")
	}
	res, _, err := c.Call(context.Background(), "lookup_vendor", map[string]any{"name": "Harbor Greens"})
	if err != nil || len(res["rows"].([]map[string]any)) != 1 {
		t.Fatalf("seed rows missing: %v %v", res, err)
	}
	c.Close()
	// Second open: tables exist, so a plain "create table" script must not run again.
	conn, err = New(cfg())
	if err != nil {
		t.Fatalf("reopening a database with a schema must not rerun init_sql: %v", err)
	}
	c = conn.(*Connector)
	if c.InitSQLApplied() {
		t.Fatal("init_sql ran on a database that already has tables")
	}
	res, _, _ = c.Call(context.Background(), "lookup_vendor", map[string]any{"name": "Harbor Greens"})
	if len(res["rows"].([]map[string]any)) != 1 {
		t.Fatal("seed rows must not be duplicated")
	}
	c.Close()
	// An empty existing file (what the acceptance suite or a volume may leave) also gets the schema.
	empty := filepath.Join(dir, "empty.db")
	os.WriteFile(empty, nil, 0o644)
	bad := cfg()
	bad["path"] = empty
	conn, err = New(bad)
	if err != nil || !conn.(*Connector).InitSQLApplied() {
		t.Fatalf("empty file must be initialised: %v", err)
	}
	conn.(*Connector).Close()
	// A missing schema file on an empty database is a startup error.
	missing := cfg()
	missing["path"] = filepath.Join(dir, "other.db")
	missing["init_sql"] = filepath.Join(dir, "nope.sql")
	if _, err := New(missing); err == nil || !strings.Contains(err.Error(), "nope.sql") {
		t.Fatalf("missing init_sql file must fail loudly: %v", err)
	}
	// A list may mix a file and inline SQL, applied in order.
	mixed := cfg()
	mixed["path"] = filepath.Join(dir, "mixed.db")
	mixed["init_sql"] = []any{schema, "insert into vendors(name, terms) values ('Cedar Ridge Farms', 'net 45')"}
	conn, err = New(mixed)
	if err != nil {
		t.Fatal(err)
	}
	res, _, _ = conn.Call(context.Background(), "lookup_vendor", map[string]any{"name": "Cedar Ridge Farms"})
	if len(res["rows"].([]map[string]any)) != 1 {
		t.Fatal("inline statement after the file did not run")
	}
	conn.(*Connector).Close()
	if _, err := New(map[string]any{"name": "ledger", "path": filepath.Join(dir, "x.db"), "init_sql": 5}); err == nil {
		t.Fatal("init_sql must be a string or list")
	}
}

func TestConfigErrors(t *testing.T) {
	base := map[string]any{"name": "ledger", "type": "sqlite", "path": ":memory:"}
	if _, err := New(map[string]any{"name": "ledger", "type": "sqlite"}); err == nil {
		t.Fatal("path is required")
	}
	bad := map[string]any{}
	for k, v := range base {
		bad[k] = v
	}
	bad["tools"] = map[string]any{"x": map[string]any{"description": "no statement"}}
	if _, err := New(bad); err == nil {
		t.Fatal("statement is required")
	}
	bad["tools"] = map[string]any{"x": map[string]any{"statement": "select 1"}}
	bad["probe"] = map[string]any{"tool": "y"}
	if _, err := New(bad); err == nil {
		t.Fatal("probe must name a known tool")
	}
	delete(bad, "probe")
	conn, err := New(bad)
	if err != nil {
		t.Fatal(err)
	}
	c := conn.(*Connector)
	defer c.Close()
	spec, ok := c.ProbeSpec()
	if !ok || spec.Tool != "ping" {
		t.Fatalf("default probe = %+v", spec)
	}
	if res, err := c.Probe(context.Background()); err != nil || len(res["rows"].([]map[string]any)) != 1 {
		t.Fatalf("default probe result = %v %v", res, err)
	}
	if v, err := bindValue(map[string]any{"a": 1}); err != nil || v != `{"a":1}` {
		t.Fatalf("objects bind as JSON text: %v %v", v, err)
	}
	if v, _ := bindValue(2.5); v != 2.5 {
		t.Fatal("non-integral floats stay floats")
	}
	if _, err := bindValue(struct{}{}); err == nil {
		t.Fatal("unsupported types must be rejected")
	}
}
