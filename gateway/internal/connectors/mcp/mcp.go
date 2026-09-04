// Package mcpconn is the built-in "mcp" connector: it spawns a stdio MCP
// server (JSON-RPC 2.0 over newline-delimited JSON: initialize, tools/list,
// tools/call) and exposes its tools as `<name>.<tool>`. Whether a tool
// writes comes from the server's readOnlyHint annotation or the
// configuration; the scope derivation is "none" (the remit needs the literal
// `<name>:*`) unless the server declares an x-kernos-scope template on the
// tool, in which case the scope is rendered from the call's arguments.
package mcpconn

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"regexp"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

// TypeName is the value of "type" in gateway.json for this connector.
const TypeName = "mcp"

// ProtocolVersion is the MCP protocol revision the client announces.
const ProtocolVersion = "2025-06-18"

// ScopeExtension is the key an MCP server puts on a tool to declare how the
// gateway derives its scope: a string or list of strings with {argument}
// placeholders.
const ScopeExtension = "x-kernos-scope"

func init() {
	connect.Register(New, TypeName)
}

// Logger is the logger the connector writes the server's stderr to. The
// gateway sets it to its redacting logger before building connectors.
var Logger = slog.Default()

type mcpTool struct {
	rawName string
	spec    connect.ToolSpec
	scopes  []string
}

// Connector is one MCP server process.
type Connector struct {
	name         string
	command      []string
	env          map[string]string
	overrides    map[string]map[string]any
	startTimeout time.Duration
	probe        connect.ProbeSpec
	hasProbe     bool

	mu    sync.Mutex
	proc  *process
	tools map[string]*mcpTool
	order []string
}

// New is the Factory for the mcp type. Configuration keys: command
// (required list), env (added to the process environment; the usual place
// for ${VAR} credentials), tools (overrides per tool: writes, description,
// contract, input_schema, scope), probe and start_timeout_seconds (default
// 15). A server that cannot be started at boot is logged, exposes only the
// tools named in the configuration, and is retried on the next call or
// probe.
func New(cfg map[string]any) (connect.Connector, error) {
	name, err := connect.ConnectorName(cfg)
	if err != nil {
		return nil, err
	}
	c := &Connector{name: name, env: map[string]string{}, overrides: map[string]map[string]any{}, tools: map[string]*mcpTool{}}
	if c.command, err = connect.StringsOf(cfg, "command"); err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if len(c.command) == 0 {
		return nil, fmt.Errorf("connector %s: command is required", name)
	}
	envRaw, err := connect.MapOf(cfg, "env")
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	for k, v := range envRaw {
		s, ok := v.(string)
		if !ok {
			return nil, fmt.Errorf("connector %s: env.%s must be a string", name, k)
		}
		c.env[k] = s
	}
	names, raws, err := connect.ToolsFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	for _, n := range names {
		c.overrides[n] = raws[n]
		c.overrides[connect.NormalizeName(n)] = raws[n]
	}
	timeout, err := connect.NumberOf(cfg, "start_timeout_seconds", 15)
	if err != nil || timeout <= 0 {
		return nil, fmt.Errorf("connector %s: start_timeout_seconds must be positive", name)
	}
	c.startTimeout = time.Duration(timeout * float64(time.Second))
	probeTool, probeArgs, probeContract, hasProbe, err := connect.ProbeFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	// Tools declared in the configuration are known even before the server
	// answers, with writes defaulting to true (the safe direction).
	for _, n := range names {
		op := connect.NormalizeName(n)
		spec, err := connect.ToolFromConfig(name, op, raws[n], true)
		if err != nil {
			return nil, fmt.Errorf("connector %s: %w", name, err)
		}
		spec.ScopeDerivation = connect.ScopeNone
		t := &mcpTool{rawName: n, spec: spec}
		if scopes, err := scopeTemplates(raws[n]["scope"]); err != nil {
			return nil, fmt.Errorf("connector %s: tool %s: %w", name, n, err)
		} else if len(scopes) > 0 {
			t.scopes = scopes
			t.spec.ScopeDerivation = connect.ScopeDeclared
		}
		c.tools[op] = t
		c.order = append(c.order, op)
	}
	ctx, cancel := context.WithTimeout(context.Background(), c.startTimeout)
	defer cancel()
	if err := c.ensureStarted(ctx); err != nil {
		Logger.Warn("mcp server could not be started at boot, will retry on demand", "connector", name, "error", err.Error())
	}
	if hasProbe {
		probeOp := connect.NormalizeName(probeTool)
		contract := connect.Contract{Required: map[string]string{}}
		if t, ok := c.tools[probeOp]; ok {
			contract = t.spec.Contract
		}
		if probeContract != nil {
			contract = *probeContract
		}
		c.probe = connect.ProbeSpec{Tool: probeOp, Args: probeArgs, Contract: contract}
	} else {
		c.probe = connect.ProbeSpec{Tool: "tools_list", Args: map[string]any{}, Contract: connect.Contract{Required: map[string]string{"tools": connect.TypeList}}}
	}
	c.hasProbe = true
	return c, nil
}

var placeholder = regexp.MustCompile(`\{([A-Za-z_][A-Za-z0-9_]*)\}`)

func scopeTemplates(raw any) ([]string, error) {
	switch v := raw.(type) {
	case nil:
		return nil, nil
	case string:
		if v == "" {
			return nil, nil
		}
		return []string{v}, nil
	case []any:
		out := make([]string, 0, len(v))
		for _, item := range v {
			s, ok := item.(string)
			if !ok {
				return nil, fmt.Errorf("%s entries must be strings", ScopeExtension)
			}
			out = append(out, s)
		}
		return out, nil
	}
	return nil, fmt.Errorf("%s must be a string or a list of strings", ScopeExtension)
}

// Name implements connect.Connector.
func (c *Connector) Name() string { return c.name }

// Tools implements connect.Connector: the tools known from the last
// successful tools/list, or the configured ones before that.
func (c *Connector) Tools() []connect.ToolSpec {
	c.mu.Lock()
	defer c.mu.Unlock()
	out := make([]connect.ToolSpec, 0, len(c.order))
	for _, op := range c.order {
		out = append(out, c.tools[op].spec)
	}
	return out
}

// ProbeSpec implements connect.ProbeDescriber.
func (c *Connector) ProbeSpec() (connect.ProbeSpec, bool) { return c.probe, c.hasProbe }

// Scopes implements connect.ScopeDeriver for tools with a declared scope
// template; tools without one report none and the gateway requires the
// literal `<name>:*`.
func (c *Connector) Scopes(toolName string, args map[string]any) ([]string, error) {
	op := connect.Operation(c.name, toolName)
	c.mu.Lock()
	t, ok := c.tools[op]
	c.mu.Unlock()
	if !ok {
		return nil, connect.Deterministic("unknown tool %s", toolName)
	}
	if len(t.scopes) == 0 {
		return nil, nil
	}
	out := make([]string, 0, len(t.scopes))
	for _, tpl := range t.scopes {
		var missing string
		rendered := placeholder.ReplaceAllStringFunc(tpl, func(m string) string {
			key := m[1 : len(m)-1]
			v, ok := args[key]
			if !ok || v == nil {
				missing = key
				return ""
			}
			switch x := v.(type) {
			case string:
				return x
			case float64:
				return strings.TrimRight(strings.TrimRight(fmt.Sprintf("%f", x), "0"), ".")
			}
			return fmt.Sprint(v)
		})
		if missing != "" {
			return nil, connect.Deterministic("argument %q needed by the scope template %q is missing", missing, tpl)
		}
		out = append(out, rendered)
	}
	return connect.UniqueSorted(out), nil
}

// Probe implements connect.Connector: the configured probe tool, or
// tools/list when none is configured.
func (c *Connector) Probe(ctx context.Context) (map[string]any, error) {
	if c.probe.Tool == "tools_list" {
		if _, ok := c.toolByOp("tools_list"); !ok {
			tools, err := c.RefreshTools(ctx)
			if err != nil {
				return nil, err
			}
			list := make([]any, 0, len(tools))
			for _, t := range tools {
				list = append(list, map[string]any{"id": t.ID, "writes": t.Writes})
			}
			return map[string]any{"tools": list}, nil
		}
	}
	result, _, err := c.Call(ctx, c.probe.Tool, c.probe.Args)
	return result, err
}

func (c *Connector) toolByOp(op string) (*mcpTool, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	t, ok := c.tools[op]
	return t, ok
}

// RefreshTools implements connect.ToolRefresher: tools/list now.
func (c *Connector) RefreshTools(ctx context.Context) ([]connect.ToolSpec, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if err := c.ensureStartedLocked(ctx); err != nil {
		return nil, err
	}
	if err := c.listToolsLocked(ctx); err != nil {
		return nil, err
	}
	out := make([]connect.ToolSpec, 0, len(c.order))
	for _, op := range c.order {
		out = append(out, c.tools[op].spec)
	}
	return out, nil
}

// Call implements connect.Connector.
func (c *Connector) Call(ctx context.Context, toolName string, args map[string]any) (map[string]any, []string, error) {
	op := connect.Operation(c.name, toolName)
	scopes, err := c.Scopes(op, args)
	if err != nil {
		return nil, nil, err
	}
	c.mu.Lock()
	if err := c.ensureStartedLocked(ctx); err != nil {
		c.mu.Unlock()
		return nil, scopes, err
	}
	t, ok := c.tools[op]
	proc := c.proc
	c.mu.Unlock()
	if !ok {
		return nil, scopes, connect.Deterministic("unknown tool %s", toolName)
	}
	if args == nil {
		args = map[string]any{}
	}
	raw, err := proc.call(ctx, "tools/call", map[string]any{"name": t.rawName, "arguments": args})
	if err != nil {
		return nil, scopes, err
	}
	var res struct {
		Content           []map[string]any `json:"content"`
		IsError           bool             `json:"isError"`
		StructuredContent map[string]any   `json:"structuredContent"`
	}
	if err := json.Unmarshal(raw, &res); err != nil {
		return nil, scopes, fmt.Errorf("mcp tools/call result is not an object: %w", err)
	}
	var texts []string
	for _, item := range res.Content {
		if item["type"] == "text" {
			if s, ok := item["text"].(string); ok {
				texts = append(texts, s)
			}
		}
	}
	text := strings.Join(texts, "\n")
	if res.IsError {
		if text == "" {
			text = "tool reported an error"
		}
		return nil, scopes, connect.Deterministic("%s", text)
	}
	if res.StructuredContent != nil {
		return res.StructuredContent, scopes, nil
	}
	content := make([]any, 0, len(res.Content))
	for _, item := range res.Content {
		content = append(content, item)
	}
	result := map[string]any{"content": content, "text": text}
	if trimmed := strings.TrimSpace(text); strings.HasPrefix(trimmed, "{") || strings.HasPrefix(trimmed, "[") {
		var parsed any
		if json.Unmarshal([]byte(trimmed), &parsed) == nil {
			result["json"] = parsed
		}
	}
	return result, scopes, nil
}

// Close stops the server process.
func (c *Connector) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.proc != nil {
		c.proc.stop()
		c.proc = nil
	}
	return nil
}

func (c *Connector) ensureStarted(ctx context.Context) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.ensureStartedLocked(ctx)
}

func (c *Connector) ensureStartedLocked(ctx context.Context) error {
	if c.proc != nil && !c.proc.dead() {
		return nil
	}
	if c.proc != nil {
		c.proc.stop()
		c.proc = nil
	}
	p, err := startProcess(c.command, c.env, c.name)
	if err != nil {
		return fmt.Errorf("mcp server start: %w", err)
	}
	ictx, cancel := context.WithTimeout(ctx, c.startTimeout)
	defer cancel()
	if _, err := p.call(ictx, "initialize", map[string]any{
		"protocolVersion": ProtocolVersion,
		"capabilities":    map[string]any{},
		"clientInfo":      map[string]any{"name": "kernos-gateway", "version": "0.1.0"},
	}); err != nil {
		p.stop()
		return fmt.Errorf("mcp initialize: %w", err)
	}
	if err := p.notify("notifications/initialized", map[string]any{}); err != nil {
		p.stop()
		return fmt.Errorf("mcp initialized notification: %w", err)
	}
	c.proc = p
	if err := c.listToolsLocked(ictx); err != nil {
		p.stop()
		c.proc = nil
		return err
	}
	Logger.Info("mcp server started", "connector", c.name, "tools", len(c.order))
	return nil
}

type listedTool struct {
	Name        string         `json:"name"`
	Description string         `json:"description"`
	InputSchema map[string]any `json:"inputSchema"`
	Annotations map[string]any `json:"annotations"`
	Scope       any            `json:"x-kernos-scope"`
}

func (c *Connector) listToolsLocked(ctx context.Context) error {
	var all []listedTool
	cursor := ""
	for {
		params := map[string]any{}
		if cursor != "" {
			params["cursor"] = cursor
		}
		raw, err := c.proc.call(ctx, "tools/list", params)
		if err != nil {
			return fmt.Errorf("mcp tools/list: %w", err)
		}
		var page struct {
			Tools      []listedTool `json:"tools"`
			NextCursor string       `json:"nextCursor"`
		}
		if err := json.Unmarshal(raw, &page); err != nil {
			return fmt.Errorf("mcp tools/list result: %w", err)
		}
		all = append(all, page.Tools...)
		if page.NextCursor == "" || page.NextCursor == cursor {
			break
		}
		cursor = page.NextCursor
	}
	tools := map[string]*mcpTool{}
	var order []string
	for _, lt := range all {
		if lt.Name == "" {
			continue
		}
		op := connect.NormalizeName(lt.Name)
		writes := true
		if ro, ok := lt.Annotations["readOnlyHint"].(bool); ok && ro {
			writes = false
		}
		override := c.overrides[lt.Name]
		if override == nil {
			override = c.overrides[op]
		}
		if override == nil {
			override = map[string]any{}
		}
		spec, err := connect.ToolFromConfig(c.name, op, override, writes)
		if err != nil {
			return err
		}
		if spec.Description == "" {
			spec.Description = lt.Description
		}
		if len(spec.InputSchema) == 0 && lt.InputSchema != nil {
			spec.InputSchema = lt.InputSchema
		}
		spec.ScopeDerivation = connect.ScopeNone
		t := &mcpTool{rawName: lt.Name, spec: spec}
		scopes, err := scopeTemplates(lt.Scope)
		if err != nil {
			return fmt.Errorf("tool %s: %w", lt.Name, err)
		}
		if declared, err := scopeTemplates(override["scope"]); err != nil {
			return fmt.Errorf("tool %s: %w", lt.Name, err)
		} else if len(declared) > 0 {
			scopes = declared
		}
		if len(scopes) > 0 {
			t.scopes = scopes
			t.spec.ScopeDerivation = connect.ScopeDeclared
		}
		if _, dup := tools[op]; dup {
			return fmt.Errorf("mcp tools %q collide on operation name %s", lt.Name, op)
		}
		tools[op] = t
		order = append(order, op)
	}
	sort.Strings(order)
	c.tools = tools
	c.order = order
	return nil
}

// process is one running MCP server with its JSON-RPC plumbing.
type process struct {
	cmd     *exec.Cmd
	stdin   io.WriteCloser
	writeMu sync.Mutex
	mu      sync.Mutex
	pending map[int64]chan rpcResponse
	nextID  int64
	done    chan struct{}
	exitErr error
	name    string
}

type rpcResponse struct {
	Result json.RawMessage `json:"result"`
	Error  *rpcError       `json:"error"`
}

type rpcError struct {
	Code    int             `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

func (e *rpcError) Error() string { return fmt.Sprintf("mcp error %d: %s", e.Code, e.Message) }

func startProcess(command []string, env map[string]string, name string) (*process, error) {
	cmd := exec.Command(command[0], command[1:]...)
	cmd.Env = os.Environ()
	for k, v := range env {
		cmd.Env = append(cmd.Env, k+"="+v)
	}
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	p := &process{cmd: cmd, stdin: stdin, pending: map[int64]chan rpcResponse{}, done: make(chan struct{}), name: name}
	go p.readLoop(stdout)
	go func() {
		sc := bufio.NewScanner(stderr)
		sc.Buffer(make([]byte, 64*1024), 1<<20)
		for sc.Scan() {
			Logger.Debug("mcp server stderr", "connector", name, "line", sc.Text())
		}
	}()
	return p, nil
}

func (p *process) readLoop(stdout io.Reader) {
	sc := bufio.NewScanner(stdout)
	sc.Buffer(make([]byte, 1<<20), 16<<20)
	for sc.Scan() {
		line := sc.Bytes()
		if len(strings.TrimSpace(string(line))) == 0 {
			continue
		}
		var msg struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Result json.RawMessage `json:"result"`
			Error  *rpcError       `json:"error"`
		}
		if err := json.Unmarshal(line, &msg); err != nil {
			Logger.Warn("mcp server sent a line that is not JSON-RPC", "connector", p.name)
			continue
		}
		if msg.Method != "" {
			if len(msg.ID) > 0 && string(msg.ID) != "null" {
				// A request from the server: this client serves none.
				_ = p.write(map[string]any{"jsonrpc": "2.0", "id": json.RawMessage(msg.ID), "error": map[string]any{"code": -32601, "message": "method not supported by kernos-gateway"}})
			}
			continue
		}
		var id int64
		if err := json.Unmarshal(msg.ID, &id); err != nil {
			continue
		}
		p.mu.Lock()
		ch, ok := p.pending[id]
		delete(p.pending, id)
		p.mu.Unlock()
		if ok {
			ch <- rpcResponse{Result: msg.Result, Error: msg.Error}
		}
	}
	err := p.cmd.Wait()
	p.mu.Lock()
	if err != nil {
		p.exitErr = err
	} else {
		p.exitErr = errors.New("mcp server exited")
	}
	pending := p.pending
	p.pending = map[int64]chan rpcResponse{}
	close(p.done)
	p.mu.Unlock()
	for _, ch := range pending {
		ch <- rpcResponse{Error: &rpcError{Code: -32000, Message: "mcp server exited"}}
	}
}

func (p *process) dead() bool {
	select {
	case <-p.done:
		return true
	default:
		return false
	}
}

func (p *process) write(msg any) error {
	b, err := json.Marshal(msg)
	if err != nil {
		return err
	}
	p.writeMu.Lock()
	defer p.writeMu.Unlock()
	_, err = p.stdin.Write(append(b, '\n'))
	return err
}

func (p *process) notify(method string, params any) error {
	return p.write(map[string]any{"jsonrpc": "2.0", "method": method, "params": params})
}

// call sends a request and waits for its response, the context or the
// process exit. Transport failures and server exits are upstream errors;
// JSON-RPC errors for bad params or unknown methods are deterministic.
func (p *process) call(ctx context.Context, method string, params any) (json.RawMessage, error) {
	if p.dead() {
		return nil, fmt.Errorf("mcp server is not running")
	}
	p.mu.Lock()
	p.nextID++
	id := p.nextID
	ch := make(chan rpcResponse, 1)
	p.pending[id] = ch
	p.mu.Unlock()
	if err := p.write(map[string]any{"jsonrpc": "2.0", "id": id, "method": method, "params": params}); err != nil {
		p.mu.Lock()
		delete(p.pending, id)
		p.mu.Unlock()
		return nil, fmt.Errorf("mcp write: %w", err)
	}
	select {
	case <-ctx.Done():
		p.mu.Lock()
		delete(p.pending, id)
		p.mu.Unlock()
		return nil, ctx.Err()
	case res := <-ch:
		if res.Error != nil {
			switch res.Error.Code {
			case -32600, -32601, -32602, -32700:
				return nil, connect.Deterministic("%v", res.Error)
			}
			return nil, res.Error
		}
		return res.Result, nil
	}
}

func (p *process) stop() {
	p.stdin.Close()
	select {
	case <-p.done:
		return
	case <-time.After(2 * time.Second):
	}
	if p.cmd.Process != nil {
		_ = p.cmd.Process.Kill()
	}
	<-p.done
}
