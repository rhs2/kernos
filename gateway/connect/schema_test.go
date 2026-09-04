package connect

import (
	"encoding/json"
	"testing"
)

func schemaOf(t *testing.T, s string) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal([]byte(s), &m); err != nil {
		t.Fatal(err)
	}
	return m
}

func valueOf(t *testing.T, s string) any {
	t.Helper()
	var v any
	if err := json.Unmarshal([]byte(s), &v); err != nil {
		t.Fatal(err)
	}
	return v
}

func TestValidateSchema(t *testing.T) {
	schema := schemaOf(t, `{"type":"object","required":["invoice_id","amount"],
	  "properties":{"invoice_id":{"type":"string","minLength":2,"maxLength":10,"pattern":"^inv-"},
	                "amount":{"type":"number","minimum":0,"maximum":10000},
	                "account":{"type":"string","enum":["5100","5200"]},
	                "lines":{"type":"array","items":{"type":"object","required":["qty"],"properties":{"qty":{"type":"integer"}}}},
	                "flag":{"type":["boolean","null"]}},
	  "additionalProperties":false}`)
	ok := valueOf(t, `{"invoice_id":"inv-1","amount":12.5,"account":"5100","lines":[{"qty":2}],"flag":null}`)
	if issues := ValidateSchema(schema, ok); len(issues) != 0 {
		t.Fatalf("valid value rejected: %v", issues)
	}
	bad := valueOf(t, `{"invoice_id":"x","amount":-1,"account":"9","lines":[{"qty":2.5}],"flag":"no","extra":1}`)
	issues := ValidateSchema(schema, bad)
	paths := map[string]bool{}
	for _, i := range issues {
		paths[i.Path] = true
	}
	for _, want := range []string{"invoice_id", "amount", "account", "lines.0.qty", "flag", "extra"} {
		if !paths[want] {
			t.Errorf("expected an issue at %q, got %v", want, issues)
		}
	}
	missing := ValidateSchema(schema, valueOf(t, `{"invoice_id":"inv-1"}`))
	if len(missing) != 1 || missing[0].Path != "amount" {
		t.Fatalf("missing required: %v", missing)
	}
	if issues := ValidateSchema(schema, "not an object"); len(issues) != 1 || issues[0].Path != "" {
		t.Fatalf("root type: %v", issues)
	}
	if issues := ValidateSchema(nil, valueOf(t, `{"anything":1}`)); len(issues) != 0 {
		t.Fatal("nil schema accepts everything")
	}
	if issues := ValidateSchema(schemaOf(t, `{"type":"object","additionalProperties":{"type":"string"}}`), valueOf(t, `{"a":1}`)); len(issues) != 1 {
		t.Fatalf("additionalProperties schema: %v", issues)
	}
	if issues := ValidateSchema(schemaOf(t, `{"type":"string","pattern":"("}`), "x"); len(issues) != 1 {
		t.Fatalf("invalid pattern must be reported, got %v", issues)
	}
	if issues := ValidateSchema(schemaOf(t, `{"type":"number"}`), int64(3)); len(issues) != 0 {
		t.Fatalf("Go integers count as numbers: %v", issues)
	}
}

func TestContractCheck(t *testing.T) {
	c := Contract{Required: map[string]string{"status": "number", "body": "string", "json": "object", "rows": "list", "ok": "bool"}}
	good := map[string]any{"status": float64(200), "body": "x", "json": map[string]any{}, "rows": []any{}, "ok": true, "extra": 1}
	if d := CheckContract(c, good); !d.OK() {
		t.Fatalf("good response failed: %s", d)
	}
	bad := map[string]any{"status": "200", "body": "x", "rows": []any{}, "ok": true}
	d := CheckContract(c, bad)
	if d.OK() || len(d.Missing) != 1 || d.Missing[0] != "json" {
		t.Fatalf("missing: %+v", d)
	}
	if mm, ok := d.TypeMismatch["status"]; !ok || mm.Expected != "number" || mm.Observed != "string" {
		t.Fatalf("type mismatch: %+v", d)
	}
	out, err := json.Marshal(Diff{})
	if err != nil || string(out) != `{"missing":[],"type_mismatch":{},"unexpected_required":[]}` {
		t.Fatalf("empty diff json = %s (%v)", out, err)
	}
	if JSONType(int64(1)) != "number" || JSONType(nil) != "null" || JSONType(json.Number("1")) != "number" {
		t.Fatal("JSONType")
	}
	shape := Shape(map[string]any{"a": map[string]any{"b": []any{}}, "c": "x"}).(map[string]any)
	if shape["c"] != "string" || shape["a"].(map[string]any)["b"] != "list" {
		t.Fatalf("Shape = %v", shape)
	}
}

func TestContractFromConfig(t *testing.T) {
	c, err := ContractFromConfig(valueOf(t, `{"required":{"entry_id":"integer","posted_at":"string","tags":"array","flag":"boolean"}}`))
	if err != nil {
		t.Fatal(err)
	}
	if c.Required["entry_id"] != "number" || c.Required["tags"] != "list" || c.Required["flag"] != "bool" {
		t.Fatalf("synonyms not normalised: %v", c.Required)
	}
	if _, err := ContractFromConfig(valueOf(t, `{"required":{"x":"blob"}}`)); err == nil {
		t.Fatal("unknown type must be rejected")
	}
	if c, err := ContractFromConfig(nil); err != nil || len(c.Required) != 0 {
		t.Fatal("nil contract is empty")
	}
}

func TestToolFromConfig(t *testing.T) {
	raw := valueOf(t, `{"description":"Post","writes":true,"input_schema":{"type":"object","required":["a"]},"contract":{"required":{"id":"number"}}}`).(map[string]any)
	spec, err := ToolFromConfig("ledger", "post_entry", raw, false)
	if err != nil {
		t.Fatal(err)
	}
	if spec.ID != "ledger.post_entry" || !spec.Writes || spec.Contract.Required["id"] != "number" || spec.InputSchema["type"] != "object" {
		t.Fatalf("spec = %+v", spec)
	}
	if _, err := ToolFromConfig("ledger", "Bad Name", raw, false); err == nil {
		t.Fatal("invalid operation name accepted")
	}
	tool, args, contract, ok, err := ProbeFromConfig(valueOf(t, `{"probe":{"tool":"get","args":{"url":"http://127.0.0.1:1/probe"},"contract":{"required":{"json":"object"}}}}`).(map[string]any))
	if err != nil || !ok || tool != "get" || args["url"] == nil || contract == nil || contract.Required["json"] != "object" {
		t.Fatalf("probe = %s %v %v %v %v", tool, args, contract, ok, err)
	}
	if _, _, _, ok, _ := ProbeFromConfig(map[string]any{}); ok {
		t.Fatal("absent probe reported present")
	}
	req := RequiredInputs("crm", []ToolSpec{{ID: "crm.search", InputSchema: map[string]any{"required": []any{"query", "limit"}}}})
	if len(req) != 2 || req[0] != "search.limit" || req[1] != "search.query" {
		t.Fatalf("RequiredInputs = %v", req)
	}
}
