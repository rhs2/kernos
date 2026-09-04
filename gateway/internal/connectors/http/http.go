// Package httpconn is the built-in "http" connector: get and post against an
// allow-list of hosts, scope `http:host:<hostname>` derived from the request
// URL, and a result of status, headers, body and json. Credentials live in
// the connector's default headers, substituted from the environment, so the
// reasoning layer never sees them.
package httpconn

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

// TypeName is the value of "type" in gateway.json for this connector.
const TypeName = "http"

func init() {
	connect.Register(New, TypeName)
}

// Connector is one allow-listed HTTP client.
type Connector struct {
	name     string
	allowed  []string
	headers  map[string]string
	tools    map[string]connect.ToolSpec
	order    []string
	client   *http.Client
	maxBody  int64
	probe    connect.ProbeSpec
	hasProbe bool
}

var defaultSchema = map[string]any{
	"type":     "object",
	"required": []any{"url"},
	"properties": map[string]any{
		"url":     map[string]any{"type": "string", "minLength": 1},
		"headers": map[string]any{"type": "object"},
		"body":    map[string]any{},
	},
}

var defaultContract = connect.Contract{Required: map[string]string{"status": connect.TypeNumber, "body": connect.TypeString}}

// New is the Factory for the http type. Configuration keys: allowed_hosts
// (required, exact names or `*.suffix`), headers (default request headers,
// where credentials go), tools (get and post overrides; when present only
// the listed tools are exposed), timeout_seconds (default 30),
// max_body_bytes (default 4 MiB) and probe.
func New(cfg map[string]any) (connect.Connector, error) {
	name, err := connect.ConnectorName(cfg)
	if err != nil {
		return nil, err
	}
	c := &Connector{name: name, tools: map[string]connect.ToolSpec{}, headers: map[string]string{}}
	if c.allowed, err = connect.StringsOf(cfg, "allowed_hosts"); err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if len(c.allowed) == 0 {
		return nil, fmt.Errorf("connector %s: allowed_hosts must list at least one host", name)
	}
	for i, h := range c.allowed {
		c.allowed[i] = strings.ToLower(strings.TrimSpace(h))
	}
	hdrs, err := connect.MapOf(cfg, "headers")
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	for k, v := range hdrs {
		s, ok := v.(string)
		if !ok {
			return nil, fmt.Errorf("connector %s: headers.%s must be a string", name, k)
		}
		c.headers[k] = s
	}
	timeout, err := connect.NumberOf(cfg, "timeout_seconds", 30)
	if err != nil || timeout <= 0 {
		return nil, fmt.Errorf("connector %s: timeout_seconds must be a positive number", name)
	}
	maxBody, err := connect.NumberOf(cfg, "max_body_bytes", 4<<20)
	if err != nil || maxBody <= 0 {
		return nil, fmt.Errorf("connector %s: max_body_bytes must be a positive number", name)
	}
	c.maxBody = int64(maxBody)
	c.client = &http.Client{
		Timeout: time.Duration(timeout * float64(time.Second)),
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	names, raws, err := connect.ToolsFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if len(names) == 0 {
		names = []string{"get", "post"}
		raws = map[string]map[string]any{"get": {}, "post": {}}
	}
	for _, op := range names {
		if op != "get" && op != "post" {
			return nil, fmt.Errorf("connector %s: unknown tool %q (only get and post exist)", name, op)
		}
		spec, err := connect.ToolFromConfig(name, op, raws[op], op == "post")
		if err != nil {
			return nil, fmt.Errorf("connector %s: %w", name, err)
		}
		if spec.Description == "" {
			spec.Description = map[string]string{"get": "HTTP GET against an allowed host", "post": "HTTP POST against an allowed host"}[op]
		}
		if len(spec.InputSchema) == 0 {
			spec.InputSchema = defaultSchema
		}
		if len(spec.Contract.Required) == 0 {
			spec.Contract = defaultContract
		}
		spec.ScopeDerivation = connect.ScopeByHost
		c.tools[op] = spec
		c.order = append(c.order, op)
	}
	probeTool, probeArgs, probeContract, hasProbe, err := connect.ProbeFromConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("connector %s: %w", name, err)
	}
	if hasProbe {
		spec, ok := c.tools[probeTool]
		if !ok {
			return nil, fmt.Errorf("connector %s: probe names unknown tool %q", name, probeTool)
		}
		c.probe = connect.ProbeSpec{Tool: probeTool, Args: probeArgs, Contract: spec.Contract}
		if probeContract != nil {
			c.probe.Contract = *probeContract
		}
		c.hasProbe = true
	}
	return c, nil
}

// Name implements connect.Connector.
func (c *Connector) Name() string { return c.name }

// Tools implements connect.Connector.
func (c *Connector) Tools() []connect.ToolSpec {
	out := make([]connect.ToolSpec, 0, len(c.order))
	for _, op := range c.order {
		out = append(out, c.tools[op])
	}
	return out
}

// ProbeSpec implements connect.ProbeDescriber.
func (c *Connector) ProbeSpec() (connect.ProbeSpec, bool) { return c.probe, c.hasProbe }

// Probe implements connect.Connector.
func (c *Connector) Probe(ctx context.Context) (map[string]any, error) {
	if !c.hasProbe {
		return nil, connect.Deterministic("connector %s has no probe configured", c.name)
	}
	result, _, err := c.Call(ctx, c.probe.Tool, c.probe.Args)
	return result, err
}

func parseURL(args map[string]any) (*url.URL, error) {
	raw, _ := args["url"].(string)
	if raw == "" {
		return nil, connect.Deterministic("url is required")
	}
	u, err := url.Parse(raw)
	if err != nil {
		return nil, connect.Deterministic("url is invalid: %v", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, connect.Deterministic("url scheme must be http or https")
	}
	if u.Hostname() == "" {
		return nil, connect.Deterministic("url has no host")
	}
	return u, nil
}

// Scopes implements connect.ScopeDeriver: the host of the request URL.
func (c *Connector) Scopes(_ string, args map[string]any) ([]string, error) {
	u, err := parseURL(args)
	if err != nil {
		return nil, err
	}
	return []string{connect.HostScope(u.Hostname())}, nil
}

// HostAllowed reports whether the allow-list admits a host name.
func (c *Connector) HostAllowed(host string) bool {
	host = strings.ToLower(host)
	for _, a := range c.allowed {
		if a == host {
			return true
		}
		if strings.HasPrefix(a, "*.") && strings.HasSuffix(host, a[1:]) {
			return true
		}
	}
	return false
}

// Call implements connect.Connector.
func (c *Connector) Call(ctx context.Context, toolName string, args map[string]any) (map[string]any, []string, error) {
	op := connect.Operation(c.name, toolName)
	if _, ok := c.tools[op]; !ok {
		return nil, nil, connect.Deterministic("unknown tool %s", toolName)
	}
	u, err := parseURL(args)
	if err != nil {
		return nil, nil, err
	}
	scopes := []string{connect.HostScope(u.Hostname())}
	if !c.HostAllowed(u.Hostname()) {
		return nil, scopes, connect.Deterministic("host %s is not in allowed_hosts", u.Hostname())
	}
	method := http.MethodGet
	var body io.Reader
	contentType := ""
	if op == "post" {
		method = http.MethodPost
		switch b := args["body"].(type) {
		case nil:
			body = bytes.NewReader(nil)
		case string:
			body = strings.NewReader(b)
			contentType = "text/plain; charset=utf-8"
		default:
			encoded, err := json.Marshal(b)
			if err != nil {
				return nil, scopes, connect.Deterministic("body is not encodable: %v", err)
			}
			body = bytes.NewReader(encoded)
			contentType = "application/json"
		}
	} else if args["body"] != nil {
		return nil, scopes, connect.Deterministic("get does not take a body")
	}
	req, err := http.NewRequestWithContext(ctx, method, u.String(), body)
	if err != nil {
		return nil, scopes, connect.Deterministic("request could not be built: %v", err)
	}
	req.Header.Set("User-Agent", "kernos-gateway/0.1.0")
	if contentType != "" {
		req.Header.Set("Content-Type", contentType)
	}
	for k, v := range c.headers {
		req.Header.Set(k, v)
	}
	if hdrs, ok := args["headers"].(map[string]any); ok {
		for k, v := range hdrs {
			s, ok := v.(string)
			if !ok {
				return nil, scopes, connect.Deterministic("header %s must be a string", k)
			}
			req.Header.Set(k, s)
		}
	}
	res, err := c.client.Do(req)
	if err != nil {
		return nil, scopes, classify(err)
	}
	defer res.Body.Close()
	data, err := io.ReadAll(io.LimitReader(res.Body, c.maxBody+1))
	if err != nil {
		return nil, scopes, fmt.Errorf("reading response body: %w", err)
	}
	if int64(len(data)) > c.maxBody {
		return nil, scopes, connect.Deterministic("response body exceeds max_body_bytes (%d)", c.maxBody)
	}
	headers := map[string]any{}
	keys := make([]string, 0, len(res.Header))
	for k := range res.Header {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		headers[k] = strings.Join(res.Header[k], ", ")
	}
	result := map[string]any{"status": res.StatusCode, "headers": headers, "body": string(data)}
	trimmed := bytes.TrimSpace(data)
	if len(trimmed) > 0 {
		var parsed any
		if json.Unmarshal(trimmed, &parsed) == nil {
			result["json"] = parsed
		}
	}
	return result, scopes, nil
}

func classify(err error) error {
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	var ue *url.Error
	if errors.As(err, &ue) {
		if ue.Timeout() {
			return fmt.Errorf("upstream timeout: %w", err)
		}
		var ne *net.OpError
		if errors.As(err, &ne) || errors.Is(err, os.ErrDeadlineExceeded) {
			return fmt.Errorf("upstream unreachable: %w", err)
		}
	}
	return fmt.Errorf("upstream error: %w", err)
}
