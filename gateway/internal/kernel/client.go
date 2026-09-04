// Package kernel is the gateway's client for the two things it needs from
// the Kernos kernel: the control-plane public key (GET /v1/keys) and
// appending tool.refused events to a run (POST /v1/runs/{id}/events). It is
// deliberately tiny; the gateway never reads run state.
package kernel

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// Actor is how the gateway identifies itself on the events it appends.
var Actor = map[string]any{"type": "gateway", "id": "kernos-gateway"}

// Keys is the body of GET /v1/keys.
type Keys struct {
	KeyID     string `json:"key_id"`
	Algorithm string `json:"algorithm"`
	PublicKey string `json:"public_key"`
}

// Client talks to one kernel.
type Client struct {
	base  string
	token string
	http  *http.Client
}

// New builds a client for the kernel at base. token is optional and sent as
// a bearer token when set; timeout bounds every request.
func New(base, token string, timeout time.Duration) *Client {
	if timeout <= 0 {
		timeout = 5 * time.Second
	}
	return &Client{base: strings.TrimRight(base, "/"), token: token, http: &http.Client{Timeout: timeout}}
}

// BaseURL returns the kernel address, for logs.
func (c *Client) BaseURL() string { return c.base }

// Error is a non-2xx answer from the kernel with the body's error code when
// it had one.
type Error struct {
	Status int
	Code   string
	Body   string
}

func (e *Error) Error() string {
	if e.Code != "" {
		return fmt.Sprintf("kernel answered %d %s", e.Status, e.Code)
	}
	return fmt.Sprintf("kernel answered %d", e.Status)
}

func (c *Client) do(ctx context.Context, method, path string, body any, headers map[string]string, out any) error {
	var reader io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("encode request: %w", err)
		}
		reader = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, reader)
	if err != nil {
		return err
	}
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	data, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	if res.StatusCode < 200 || res.StatusCode > 299 {
		e := &Error{Status: res.StatusCode, Body: string(data)}
		var parsed struct {
			Error struct {
				Code string `json:"code"`
			} `json:"error"`
		}
		if json.Unmarshal(data, &parsed) == nil {
			e.Code = parsed.Error.Code
		}
		return e
	}
	if out != nil && len(data) > 0 {
		if err := json.Unmarshal(data, out); err != nil {
			return fmt.Errorf("decode response: %w", err)
		}
	}
	return nil
}

// FetchKeys returns the control-plane signing key.
func (c *Client) FetchKeys(ctx context.Context) (Keys, error) {
	var k Keys
	if err := c.do(ctx, http.MethodGet, "/v1/keys", nil, nil, &k); err != nil {
		return Keys{}, err
	}
	if k.PublicKey == "" {
		return Keys{}, fmt.Errorf("kernel returned no public key")
	}
	return k, nil
}

// PostEvent appends an external event to a run on behalf of the gateway. The
// remit token goes in X-Kernos-Remit, which is how the kernel authorises the
// gateway for that run.
func (c *Client) PostEvent(ctx context.Context, runID, remitToken, kind string, payload map[string]any) error {
	body := map[string]any{"kind": kind, "payload": payload, "actor": Actor}
	headers := map[string]string{}
	if remitToken != "" {
		headers["X-Kernos-Remit"] = remitToken
	}
	return c.do(ctx, http.MethodPost, "/v1/runs/"+runID+"/events", body, headers, nil)
}

// PostRefused appends the tool.refused event of 01-EVENTS.
func (c *Client) PostRefused(ctx context.Context, runID, remitToken string, payload map[string]any) error {
	return c.PostEvent(ctx, runID, remitToken, "tool.refused", payload)
}
