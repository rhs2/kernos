package connect

import (
	"context"
	"errors"
	"fmt"
	"testing"
)

func TestMatchPattern(t *testing.T) {
	cases := []struct {
		pattern, value string
		want           bool
	}{
		{"ledger.*", "ledger.post_entry", true},
		{"ledger.*", "ledgerx.post_entry", false},
		{"ledger.post_entry", "ledger.post_entry", true},
		{"ledger.post_entry", "ledger.post_entry2", false},
		{"sql:table:*", "sql:table:invoices", true},
		{"sql:table:invoices", "sql:table:Invoices", false},
		{"*", "anything", true},
		{"http:host:api.halcyon.example", "http:host:api.halcyon.example", true},
	}
	for _, c := range cases {
		if got := MatchPattern(c.pattern, c.value); got != c.want {
			t.Errorf("MatchPattern(%q, %q) = %v, want %v", c.pattern, c.value, got, c.want)
		}
	}
	if !MatchAny([]string{"a", "b*"}, "bcd") || MatchAny([]string{"a"}, "b") {
		t.Fatal("MatchAny")
	}
}

func TestScopeStrings(t *testing.T) {
	if TableScope("vendors") != "sql:table:vendors" {
		t.Fatal("table scope")
	}
	if HostScope("API.Halcyon.example") != "http:host:api.halcyon.example" {
		t.Fatal("host scope must be lowercase")
	}
	if PathScope("/srv/data") != "fs:path:/srv/data" || LiteralScope("crm") != "crm:*" {
		t.Fatal("path or literal scope")
	}
	if got := UniqueSorted([]string{"b", "a", "b"}); len(got) != 2 || got[0] != "a" {
		t.Fatalf("UniqueSorted = %v", got)
	}
}

func TestNames(t *testing.T) {
	if !ValidName("post_entry") || ValidName("Post") || ValidName("1x") || ValidName("") {
		t.Fatal("ValidName")
	}
	if NormalizeName("Search-Contacts") != "search_contacts" || NormalizeName("9lives") != "t_9lives" {
		t.Fatal("NormalizeName")
	}
	if Operation("ledger", "ledger.post_entry") != "post_entry" || Operation("ledger", "post_entry") != "post_entry" {
		t.Fatal("Operation")
	}
	if ToolID("ledger", "post_entry") != "ledger.post_entry" {
		t.Fatal("ToolID")
	}
}

func TestDeterministic(t *testing.T) {
	err := Deterministic("host %s not allowed", "x")
	if !IsDeterministic(err) || !errors.Is(err, ErrDeterministic) {
		t.Fatal("Deterministic must wrap ErrDeterministic")
	}
	if IsDeterministic(fmt.Errorf("plain")) {
		t.Fatal("plain errors are not deterministic")
	}
	wrapped := fmt.Errorf("outer: %w", err)
	if !IsDeterministic(wrapped) {
		t.Fatal("wrapping must preserve determinism")
	}
}

type fakeConnector struct{}

func (fakeConnector) Name() string      { return "fake" }
func (fakeConnector) Tools() []ToolSpec { return nil }
func (fakeConnector) Call(context.Context, string, map[string]any) (map[string]any, []string, error) {
	return nil, nil, nil
}
func (fakeConnector) Probe(context.Context) (map[string]any, error) { return nil, nil }

func TestRegistry(t *testing.T) {
	Register(func(map[string]any) (Connector, error) { return fakeConnector{}, nil }, "fake_for_test")
	f, ok := Lookup("fake_for_test")
	if !ok {
		t.Fatal("factory not registered")
	}
	c, err := f(nil)
	if err != nil || c.Name() != "fake" {
		t.Fatal("factory failed")
	}
	found := false
	for _, name := range Types() {
		if name == "fake_for_test" {
			found = true
		}
	}
	if !found {
		t.Fatal("Types must list the registered type")
	}
	if _, ok := Lookup("missing"); ok {
		t.Fatal("unknown type must not resolve")
	}
}

func TestCallInfo(t *testing.T) {
	ctx := WithCallInfo(context.Background(), CallInfo{RunID: "run_1", Step: "post", IdempotencyKey: "k"})
	if got := CallInfoFrom(ctx); got.RunID != "run_1" || got.Step != "post" || got.IdempotencyKey != "k" {
		t.Fatalf("CallInfoFrom = %+v", got)
	}
	if got := CallInfoFrom(context.Background()); got != (CallInfo{}) {
		t.Fatal("missing info must be the zero value")
	}
}
