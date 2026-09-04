package mcpconn

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

// TestMain doubles as a tiny stdio MCP server when the gateway spawns the
// test binary with KERNOS_MCP_TEST_SERVER=1.
func TestMain(m *testing.M) {
	if os.Getenv("KERNOS_MCP_TEST_SERVER") == "1" {
		fakeServer()
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func fakeServer() {
	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 1<<20), 1<<20)
	out := bufio.NewWriter(os.Stdout)
	reply := func(id any, result any) {
		b, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": id, "result": result})
		out.Write(append(b, '\n'))
		out.Flush()
	}
	fail := func(id any, code int, msg string) {
		b, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": id, "error": map[string]any{"code": code, "message": msg}})
		out.Write(append(b, '\n'))
		out.Flush()
	}
	fmt.Fprintln(os.Stderr, "fake mcp server up")
	for in.Scan() {
		var req struct {
			ID     any            `json:"id"`
			Method string         `json:"method"`
			Params map[string]any `json:"params"`
		}
		if json.Unmarshal(in.Bytes(), &req) != nil {
			continue
		}
		switch req.Method {
		case "initialize":
			reply(req.ID, map[string]any{"protocolVersion": ProtocolVersion, "capabilities": map[string]any{"tools": map[string]any{}}, "serverInfo": map[string]any{"name": "halcyon-crm-fake", "version": "0.0.1"}})
			// A server-initiated request the client must answer without dying.
			b, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
			out.Write(append(b, '\n'))
			out.Flush()
		case "notifications/initialized":
		case "tools/list":
			echoRequired := []any{"text"}
			if drift := os.Getenv("KERNOS_MCP_TEST_DRIFT_FILE"); drift != "" {
				if _, err := os.Stat(drift); err == nil {
					echoRequired = append(echoRequired, "mode")
				}
			}
			cursor, _ := req.Params["cursor"].(string)
			if cursor == "" {
				reply(req.ID, map[string]any{"tools": []any{
					map[string]any{"name": "echo", "description": "Echo the arguments", "inputSchema": map[string]any{"type": "object", "required": echoRequired}, "annotations": map[string]any{"readOnlyHint": true}},
					map[string]any{"name": "Write-Thing", "description": "Writes", "inputSchema": map[string]any{"type": "object"}},
				}, "nextCursor": "page2"})
			} else {
				reply(req.ID, map[string]any{"tools": []any{
					map[string]any{"name": "boom", "inputSchema": map[string]any{"type": "object"}},
					map[string]any{"name": "scoped", "inputSchema": map[string]any{"type": "object", "required": []any{"account"}}, "annotations": map[string]any{"readOnlyHint": true}, "x-kernos-scope": "crm:account:{account}"},
					map[string]any{"name": "crash", "inputSchema": map[string]any{"type": "object"}},
					map[string]any{"name": "slow", "inputSchema": map[string]any{"type": "object"}},
					map[string]any{"name": "textual", "annotations": map[string]any{"readOnlyHint": true}},
				}})
			}
		case "tools/call":
			name, _ := req.Params["name"].(string)
			args, _ := req.Params["arguments"].(map[string]any)
			switch name {
			case "echo":
				reply(req.ID, map[string]any{"content": []any{map[string]any{"type": "text", "text": "echoed"}}, "structuredContent": map[string]any{"echo": args}})
			case "Write-Thing":
				reply(req.ID, map[string]any{"content": []any{map[string]any{"type": "text", "text": `{"written": true}`}}})
			case "textual":
				reply(req.ID, map[string]any{"content": []any{map[string]any{"type": "text", "text": "plain words"}}})
			case "boom":
				reply(req.ID, map[string]any{"content": []any{map[string]any{"type": "text", "text": "boom: bad input"}}, "isError": true})
			case "scoped":
				reply(req.ID, map[string]any{"structuredContent": map[string]any{"account": args["account"]}})
			case "crash":
				os.Exit(3)
			case "slow":
				time.Sleep(2 * time.Second)
				reply(req.ID, map[string]any{"structuredContent": map[string]any{"late": true}})
			default:
				fail(req.ID, -32602, "unknown tool "+name)
			}
		default:
			fail(req.ID, -32601, "method not found")
		}
	}
}

func newConn(t *testing.T, extra map[string]any) *Connector {
	t.Helper()
	cfg := map[string]any{
		"name":    "crm",
		"type":    "mcp",
		"command": []any{os.Args[0]},
		"env":     map[string]any{"KERNOS_MCP_TEST_SERVER": "1"},
	}
	for k, v := range extra {
		cfg[k] = v
	}
	conn, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	c := conn.(*Connector)
	t.Cleanup(func() { c.Close() })
	return c
}

func TestClient(t *testing.T) {
	c := newConn(t, map[string]any{"tools": map[string]any{"boom": map[string]any{"writes": false, "contract": map[string]any{"required": map[string]any{"x": "string"}}}}})
	tools := c.Tools()
	ids := map[string]connect.ToolSpec{}
	for _, tl := range tools {
		ids[tl.ID] = tl
	}
	if len(tools) != 7 || tools[0].ID != "crm.boom" || tools[1].ID != "crm.crash" {
		t.Fatalf("tools = %+v", tools)
	}
	if ids["crm.echo"].Writes || ids["crm.echo"].Description != "Echo the arguments" || ids["crm.echo"].InputSchema["required"].([]any)[0] != "text" {
		t.Fatalf("echo spec = %+v", ids["crm.echo"])
	}
	if !ids["crm.write_thing"].Writes || ids["crm.write_thing"].ScopeDerivation != connect.ScopeNone {
		t.Fatalf("write_thing spec = %+v", ids["crm.write_thing"])
	}
	if ids["crm.boom"].Writes || ids["crm.boom"].Contract.Required["x"] != "string" {
		t.Fatalf("config overrides not applied: %+v", ids["crm.boom"])
	}
	if ids["crm.scoped"].ScopeDerivation != connect.ScopeDeclared {
		t.Fatalf("scoped spec = %+v", ids["crm.scoped"])
	}
	ctx := context.Background()

	scopes, err := c.Scopes("echo", map[string]any{"text": "hi"})
	if err != nil || scopes != nil {
		t.Fatalf("undeclared scope must derive nothing: %v %v", scopes, err)
	}
	scopes, err = c.Scopes("crm.scoped", map[string]any{"account": "acc-42"})
	if err != nil || len(scopes) != 1 || scopes[0] != "crm:account:acc-42" {
		t.Fatalf("declared scope = %v %v", scopes, err)
	}
	if scopes, _ := c.Scopes("scoped", map[string]any{"account": float64(7)}); scopes[0] != "crm:account:7" {
		t.Fatalf("numbers render without a fraction: %v", scopes)
	}
	if _, err := c.Scopes("scoped", map[string]any{}); !connect.IsDeterministic(err) {
		t.Fatalf("missing template argument: %v", err)
	}
	if _, err := c.Scopes("nope", nil); !connect.IsDeterministic(err) {
		t.Fatal("unknown tool scope")
	}

	result, _, err := c.Call(ctx, "echo", map[string]any{"text": "hi"})
	if err != nil || result["echo"].(map[string]any)["text"] != "hi" {
		t.Fatalf("echo = %v %v", result, err)
	}
	result, _, err = c.Call(ctx, "crm.write_thing", nil)
	if err != nil || result["text"] != `{"written": true}` || result["json"].(map[string]any)["written"] != true || len(result["content"].([]any)) != 1 {
		t.Fatalf("write_thing = %v %v", result, err)
	}
	result, _, err = c.Call(ctx, "textual", nil)
	if err != nil || result["text"] != "plain words" || result["json"] != nil {
		t.Fatalf("textual = %v %v", result, err)
	}
	_, _, err = c.Call(ctx, "boom", map[string]any{})
	if !connect.IsDeterministic(err) || !strings.Contains(err.Error(), "boom: bad input") {
		t.Fatalf("isError must be deterministic: %v", err)
	}
	if _, _, err := c.Call(ctx, "missing", nil); !connect.IsDeterministic(err) {
		t.Fatalf("unknown tool: %v", err)
	}

	probe, err := c.Probe(ctx)
	if err != nil || connect.JSONType(probe["tools"]) != "list" || len(probe["tools"].([]any)) != 7 {
		t.Fatalf("default probe = %v %v", probe, err)
	}
	spec, ok := c.ProbeSpec()
	if !ok || spec.Tool != "tools_list" || spec.Contract.Required["tools"] != "list" {
		t.Fatalf("ProbeSpec = %+v", spec)
	}

	cctx, cancel := context.WithTimeout(ctx, 100*time.Millisecond)
	defer cancel()
	_, _, err = c.Call(cctx, "slow", nil)
	if err == nil || connect.IsDeterministic(err) {
		t.Fatalf("timeout is an upstream error: %v", err)
	}
}

func TestRestartAfterCrash(t *testing.T) {
	c := newConn(t, nil)
	ctx := context.Background()
	_, _, err := c.Call(ctx, "crash", nil)
	if err == nil || connect.IsDeterministic(err) {
		t.Fatalf("a crash is an upstream error: %v", err)
	}
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) && !c.proc.dead() {
		time.Sleep(10 * time.Millisecond)
	}
	result, _, err := c.Call(ctx, "echo", map[string]any{"text": "again"})
	if err != nil || result["echo"].(map[string]any)["text"] != "again" {
		t.Fatalf("the server must be restarted on the next call: %v %v", result, err)
	}
}

func TestDriftAndConfiguredProbe(t *testing.T) {
	drift := filepath.Join(t.TempDir(), "drift")
	c := newConn(t, map[string]any{
		"env":   map[string]any{"KERNOS_MCP_TEST_SERVER": "1", "KERNOS_MCP_TEST_DRIFT_FILE": drift},
		"probe": map[string]any{"tool": "echo", "args": map[string]any{"text": "probe"}, "contract": map[string]any{"required": map[string]any{"echo": "object"}}},
	})
	before := connect.RequiredInputs("crm", c.Tools())
	os.WriteFile(drift, []byte("x"), 0o644)
	tools, err := c.RefreshTools(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	after := connect.RequiredInputs("crm", tools)
	if len(after) != len(before)+1 || after[0] != "echo.mode" {
		t.Fatalf("drift not visible: before %v after %v", before, after)
	}
	probe, err := c.Probe(context.Background())
	if err != nil || probe["echo"].(map[string]any)["text"] != "probe" {
		t.Fatalf("configured probe = %v %v", probe, err)
	}
	spec, _ := c.ProbeSpec()
	if spec.Tool != "echo" || spec.Contract.Required["echo"] != "object" {
		t.Fatalf("ProbeSpec = %+v", spec)
	}
}

func TestStartFailure(t *testing.T) {
	cfg := map[string]any{"name": "crm", "command": []any{filepath.Join(t.TempDir(), "missing-binary")},
		"tools": map[string]any{"search": map[string]any{"description": "Search", "scope": "crm:search:*"}}}
	conn, err := New(cfg)
	if err != nil {
		t.Fatalf("a server that cannot start must not fail the build: %v", err)
	}
	c := conn.(*Connector)
	tools := c.Tools()
	if len(tools) != 1 || tools[0].ID != "crm.search" || !tools[0].Writes || tools[0].ScopeDerivation != connect.ScopeDeclared {
		t.Fatalf("configured tools must be known: %+v", tools)
	}
	if _, _, err := c.Call(context.Background(), "search", nil); err == nil || connect.IsDeterministic(err) {
		t.Fatalf("calls fail upstream until the server starts: %v", err)
	}
	if _, err := c.Probe(context.Background()); err == nil {
		t.Fatal("probe must fail")
	}
	if _, err := New(map[string]any{"name": "crm"}); err == nil {
		t.Fatal("command is required")
	}
}
